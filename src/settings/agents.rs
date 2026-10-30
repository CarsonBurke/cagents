//! Provider launch preferences shared by the CLI, daemon and native composer.
use crate::{agent::Provider, model::LaunchRecipe};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::Path,
};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AgentLaunchSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub executable: Option<String>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub(crate) unreadable_execution: UnreadableExecution,
}

impl std::fmt::Debug for AgentLaunchSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentLaunchSettings")
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("permission_mode", &self.permission_mode)
            .field("executable", &self.executable)
            .field("arguments", &"<redacted>")
            .field("environment", &"<redacted>")
            .field("unreadable_execution", &self.unreadable_execution)
            .finish()
    }
}

/// Retain unreadable execution leaves under their original saved keys. A
/// string envelope also gives NaN and other TOML values stable equality.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct UnreadableExecution(
    BTreeMap<String, String>,
    Option<UnreadablePolicyReset>,
    BTreeSet<String>,
);

impl std::fmt::Debug for UnreadableExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnreadableExecution")
            .field("fields", &self.0.keys().collect::<Vec<_>>())
            .field("container", &self.1)
            .field("repairs", &self.2)
            .finish()
    }
}

/// An exact observed container, carried only by a local Settings action.
/// Its original value is private and never appears in messages or feedback.
#[derive(Clone, PartialEq, Eq)]
pub struct UnreadablePolicyReset {
    all_providers: bool,
    field: Option<String>,
    document: String,
}

impl std::fmt::Debug for UnreadablePolicyReset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnreadablePolicyReset")
            .field("all_providers", &self.all_providers)
            .field("field", &self.field)
            .finish_non_exhaustive()
    }
}

impl UnreadablePolicyReset {
    pub(crate) fn all_providers(&self) -> bool {
        self.all_providers
    }

    pub(crate) fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    fn value(&self) -> Result<toml::Value, toml::de::Error> {
        Ok(toml::from_str::<toml::Table>(&self.document)?
            .remove("value")
            .expect("internally retained value"))
    }
}

pub(super) struct UnreadableAgentContainers(Vec<(String, UnreadablePolicyReset)>);

impl UnreadableAgentContainers {
    pub(super) fn restore(self, settings: &mut BTreeMap<String, AgentLaunchSettings>) {
        for (provider, container) in self.0 {
            settings.entry(provider).or_default().unreadable_execution.1 = Some(container);
        }
    }
}

/// Decode unreadable containers without inferring any launch policy from them.
/// Nonexecution fields inside valid tables still use ordinary granular fallback.
pub(super) fn retain_unreadable_containers(
    table: &mut toml::Table,
) -> Result<UnreadableAgentContainers, toml::ser::Error> {
    let mut retained = Vec::new();
    if let Some(agents) = table.get_mut("agents") {
        if let Some(agents) = agents.as_table_mut() {
            for provider in Provider::ALL {
                let key = provider_key(provider);
                if let Some(value) = agents.get_mut(key)
                    && !value.is_table()
                {
                    retained.push((
                        key.into(),
                        UnreadablePolicyReset {
                            all_providers: false,
                            field: None,
                            document: toml::to_string(&BTreeMap::from([("value", value.clone())]))?,
                        },
                    ));
                    *value = toml::Value::Table(toml::Table::new());
                }
            }
        } else {
            let container = UnreadablePolicyReset {
                all_providers: true,
                field: None,
                document: toml::to_string(&BTreeMap::from([("value", agents.clone())]))?,
            };
            *agents = toml::Value::Table(toml::Table::new());
            for provider in Provider::ALL {
                retained.push((provider_key(provider).into(), container.clone()));
            }
        }
    }
    Ok(UnreadableAgentContainers(retained))
}

pub(super) fn serialize_provider_settings<S: serde::Serializer>(
    settings: &BTreeMap<String, AgentLaunchSettings>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if let Some(container) = settings
        .values()
        .filter_map(AgentLaunchSettings::unreadable_container)
        .find(UnreadablePolicyReset::all_providers)
    {
        if !settings
            .values()
            .all(|configuration| configuration.unreadable_execution.1.as_ref() == Some(&container))
        {
            return Err(serde::ser::Error::custom(
                "Reset the unreadable provider preferences before editing this section.",
            ));
        }
        if settings
            .values()
            .all(AgentLaunchSettings::container_repair_requested)
        {
            return BTreeMap::<String, AgentLaunchSettings>::new().serialize(serializer);
        }
        if settings
            .values()
            .any(AgentLaunchSettings::container_repair_requested)
        {
            return Err(serde::ser::Error::custom(
                "The provider preference repair is incomplete.",
            ));
        }
        return container
            .value()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer);
    }
    settings.serialize(serializer)
}

pub(crate) fn repair_unreadable_policy(
    settings: &mut BTreeMap<String, AgentLaunchSettings>,
    provider: Provider,
    observed: &UnreadablePolicyReset,
) -> bool {
    if let Some(field) = observed.field() {
        let Some(configuration) = settings.get_mut(provider_key(provider)) else {
            return false;
        };
        if configuration.unreadable_container().is_some()
            || configuration.unreadable_field(field).as_ref() != Some(observed)
        {
            return false;
        }
        match field {
            "executable" => configuration.set_executable(None),
            "arguments" => configuration.set_arguments(configuration.arguments.clone()),
            "environment" => configuration.set_environment(configuration.environment.clone()),
            _ => return false,
        }
        return true;
    }
    if settings
        .get(provider_key(provider))
        .and_then(AgentLaunchSettings::unreadable_container)
        .as_ref()
        != Some(observed)
    {
        return false;
    }
    if observed.all_providers {
        if !settings
            .values()
            .all(|configuration| configuration.unreadable_execution.1.as_ref() == Some(observed))
        {
            return false;
        }
        for configuration in settings.values_mut() {
            configuration
                .unreadable_execution
                .2
                .insert("container".into());
        }
    } else {
        settings
            .get_mut(provider_key(provider))
            .expect("observed provider")
            .unreadable_execution
            .2
            .insert("container".into());
    }
    true
}

pub(crate) fn has_policy_repairs(settings: &BTreeMap<String, AgentLaunchSettings>) -> bool {
    settings
        .values()
        .any(|configuration| !configuration.unreadable_execution.2.is_empty())
}

fn same_field(left: &AgentLaunchSettings, right: &AgentLaunchSettings, field: &str) -> bool {
    match field {
        "executable" => left.executable == right.executable,
        "arguments" => left.arguments == right.arguments,
        "environment" => left.environment == right.environment,
        _ => false,
    }
}

/// Confirm only the repaired scope; unrelated concurrent preferences may
/// legitimately differ from this window's captured desired settings.
pub(crate) fn confirm_policy_repairs(
    requested: &BTreeMap<String, AgentLaunchSettings>,
    persisted: &BTreeMap<String, AgentLaunchSettings>,
) -> Result<(), String> {
    for (provider, configuration) in requested {
        if configuration.container_repair_requested() {
            let observed = configuration
                .unreadable_execution
                .1
                .as_ref()
                .expect("requested container");
            if observed.all_providers {
                if !persisted.is_empty() {
                    return Err("Provider preference repair changed before it could be acknowledged. Restart the app to load the changed preferences.".into());
                }
            } else if persisted.get(provider) != Some(&AgentLaunchSettings::default()) {
                return Err("Provider preference repair changed before it could be acknowledged. Restart the app to load the changed preferences.".into());
            }
        }
        for field in &configuration.unreadable_execution.2 {
            if field == "container" {
                continue;
            }
            if persisted.get(provider).is_none_or(|current| {
                current.unreadable_container().is_some()
                    || current.unreadable_field(field).is_some()
                    || !same_field(configuration, current, field)
            }) {
                return Err("Provider preference repair changed before it could be acknowledged. Restart the app to load the changed preferences.".into());
            }
        }
    }
    Ok(())
}

/// Complete only an acknowledged repair of the same raw evidence and value.
/// A newer edit or newly observed unreadable policy keeps its own refusal.
pub(crate) fn acknowledge_policy_repairs(
    current: &mut BTreeMap<String, AgentLaunchSettings>,
    accepted: &BTreeMap<String, AgentLaunchSettings>,
) {
    if accepted.values().any(|configuration| {
        configuration.container_repair_requested()
            && configuration
                .unreadable_execution
                .1
                .as_ref()
                .is_some_and(UnreadablePolicyReset::all_providers)
    }) {
        if current == accepted {
            current.clear();
        }
        return;
    }
    for (provider, accepted) in accepted {
        let Some(configuration) = current.get_mut(provider) else {
            continue;
        };
        if accepted.container_repair_requested() && configuration == accepted {
            configuration.unreadable_execution.1 = None;
            configuration.unreadable_execution.2.remove("container");
        }
        for field in &accepted.unreadable_execution.2 {
            if field != "container"
                && configuration.unreadable_execution.2.contains(field)
                && configuration.unreadable_execution.0.get(field)
                    == accepted.unreadable_execution.0.get(field)
                && same_field(configuration, accepted, field)
            {
                configuration.unreadable_execution.0.remove(field);
                configuration.unreadable_execution.2.remove(field);
            }
        }
    }
}

pub(crate) fn complete_policy_repairs(settings: &mut BTreeMap<String, AgentLaunchSettings>) {
    let accepted = settings.clone();
    acknowledge_policy_repairs(settings, &accepted);
}

/// A write may have succeeded before its confirmation failed. On retry, an
/// exact canonical match needs only confirmation, never another replacement.
/// Different current policy remains protected from the old repair request.
pub(super) fn policy_repair_merge_base(
    base: &BTreeMap<String, AgentLaunchSettings>,
    requested: &BTreeMap<String, AgentLaunchSettings>,
    current: &BTreeMap<String, AgentLaunchSettings>,
) -> Result<BTreeMap<String, AgentLaunchSettings>, String> {
    let changed = || {
        "Provider preferences changed before this repair was acknowledged. Restart the app to load the changed preferences.".to_owned()
    };
    let mut merged_base = base.clone();
    for (provider, configuration) in requested {
        let Some(previous) = base.get(provider) else {
            if configuration.unreadable_execution.2.is_empty() {
                continue;
            }
            return Err(changed());
        };
        if configuration.container_repair_requested() {
            let observed = configuration
                .unreadable_execution
                .1
                .as_ref()
                .expect("requested container");
            if previous.unreadable_execution.1.as_ref() != Some(observed) {
                return Err(changed());
            }
            if observed.all_providers {
                let original = previous.stored_unreadable_container().as_ref() == Some(observed)
                    && current.len() == Provider::ALL.len()
                    && current
                        .values()
                        .all(|current| current.unreadable_execution.1.as_ref() == Some(observed));
                if original {
                    return Ok(merged_base);
                }
                if current.is_empty() {
                    return Ok(BTreeMap::new());
                }
                return Err(changed());
            }
            let actual = current.get(provider).ok_or_else(changed)?;
            if previous.stored_unreadable_container().as_ref() == Some(observed)
                && actual.unreadable_execution.1.as_ref() == Some(observed)
            {
                continue;
            }
            if actual != &AgentLaunchSettings::default() {
                return Err(changed());
            }
            merged_base.insert(provider.clone(), AgentLaunchSettings::default());
            continue;
        }
        for field in &configuration.unreadable_execution.2 {
            let observed = configuration
                .unreadable_execution
                .0
                .get(field)
                .ok_or_else(changed)?;
            if previous.unreadable_execution.0.get(field) != Some(observed) {
                return Err(changed());
            }
            let actual = current.get(provider).ok_or_else(changed)?;
            if actual.unreadable_container().is_some() {
                return Err(changed());
            }
            if let Some(raw) = actual.stored_unreadable_field(field) {
                if previous.stored_unreadable_field(field) == Some(observed) && raw == observed {
                    continue;
                }
                return Err(changed());
            }
            if !same_field(configuration, actual, field) {
                return Err(changed());
            }
            let baseline = merged_base.get_mut(provider).expect("observed provider");
            match field.as_str() {
                "executable" => baseline.executable.clone_from(&actual.executable),
                "arguments" => baseline.arguments.clone_from(&actual.arguments),
                "environment" => baseline.environment.clone_from(&actual.environment),
                _ => return Err(changed()),
            }
            baseline.unreadable_execution.0.remove(field);
            baseline.unreadable_execution.2.remove(field);
        }
    }
    Ok(merged_base)
}

/// An old window may edit other preferences, but cannot replace a newly
/// unreadable container or apply a reset after that exact container changed.
pub(super) fn validate_container_repairs(
    base: &BTreeMap<String, AgentLaunchSettings>,
    desired: &BTreeMap<String, AgentLaunchSettings>,
    current: &BTreeMap<String, AgentLaunchSettings>,
) -> Result<(), String> {
    let global = |settings: &BTreeMap<String, AgentLaunchSettings>| {
        settings
            .values()
            .filter_map(AgentLaunchSettings::stored_unreadable_container)
            .find(UnreadablePolicyReset::all_providers)
    };
    let (base_global, current_global) = (global(base), global(current));
    if (base_global.is_some() || current_global.is_some()) && base != desired {
        let requested = base_global.as_ref().is_some_and(|observed| {
            desired.values().all(|configuration| {
                configuration.unreadable_execution.1.as_ref() == Some(observed)
                    && configuration.container_repair_requested()
            })
        });
        if base_global.is_none()
            || base_global != current_global
            || (!desired.is_empty() && !requested)
        {
            return Err("Provider preferences changed or are unreadable. Restart the app to load the changed preferences before resetting this section.".into());
        }
        return Ok(());
    }
    for provider in Provider::ALL {
        let key = provider_key(provider);
        if base.get(key) == desired.get(key) {
            continue;
        }
        let base_container = base
            .get(key)
            .and_then(AgentLaunchSettings::stored_unreadable_container);
        let current_container = current
            .get(key)
            .and_then(AgentLaunchSettings::stored_unreadable_container);
        let requested = desired.get(key).is_some_and(|configuration| {
            configuration.container_repair_requested()
                && configuration.unreadable_execution.1 == base_container
        });
        if (base_container.is_some() || current_container.is_some())
            && (base_container.is_none()
                || base_container != current_container
                || (desired.contains_key(key) && !requested))
        {
            return Err(format!(
                "{} preferences changed or are unreadable. Restart the app to load the changed preferences before resetting this provider.",
                provider.label()
            ));
        }
        let empty = AgentLaunchSettings::default();
        let base = base.get(key).unwrap_or(&empty);
        let desired = desired.get(key).unwrap_or(&empty);
        let current = current.get(key).unwrap_or(&empty);
        let base_value = toml::Value::try_from(base).map_err(|error| error.to_string())?;
        let desired_value = toml::Value::try_from(desired).map_err(|error| error.to_string())?;
        for field in ["executable", "arguments", "environment"] {
            let observed = base.stored_unreadable_field(field);
            let actual = current.stored_unreadable_field(field);
            if (observed.is_some() || actual.is_some())
                && match (base_value.get(field), desired_value.get(field)) {
                    (Some(base), Some(desired)) => !super::same_toml(base, desired),
                    (None, None) => false,
                    _ => true,
                }
                && (observed.is_none() || observed != actual)
            {
                return Err(format!(
                    "{} saved {field} changed or is unreadable. Restart the app to load the changed preferences before repairing it.",
                    provider.label()
                ));
            }
        }
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SavedAgentLaunchSettings {
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    executable: Option<toml::Value>,
    arguments: Option<toml::Value>,
    environment: Option<toml::Value>,
}

impl UnreadableExecution {
    fn decode<T: serde::de::DeserializeOwned + Default>(
        &mut self,
        field: &str,
        value: Option<toml::Value>,
        retained_entries: T,
    ) -> Result<T, toml::ser::Error> {
        let Some(value) = value else {
            return Ok(T::default());
        };
        if let Ok(decoded) = value.clone().try_into::<T>() {
            return Ok(decoded);
        }
        self.0.insert(
            field.into(),
            toml::to_string(&BTreeMap::from([("value", value)]))?,
        );
        Ok(retained_entries)
    }

    fn saved(
        &self,
        field: &str,
        fallback: Option<toml::Value>,
    ) -> Result<Option<toml::Value>, toml::de::Error> {
        match self.0.get(field).filter(|_| !self.2.contains(field)) {
            Some(document) => Ok(toml::from_str::<toml::Table>(document)?.remove("value")),
            None => Ok(fallback),
        }
    }
}

impl<'de> Deserialize<'de> for AgentLaunchSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let saved = SavedAgentLaunchSettings::deserialize(deserializer)?;
        let arguments = saved
            .arguments
            .as_ref()
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let environment = saved
            .environment
            .as_ref()
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.into())))
            .collect::<BTreeMap<_, _>>();
        let mut unreadable_execution = UnreadableExecution::default();
        Ok(Self {
            model: saved.model,
            effort: saved.effort,
            permission_mode: saved.permission_mode,
            executable: unreadable_execution
                .decode("executable", saved.executable, None)
                .map_err(serde::de::Error::custom)?,
            arguments: unreadable_execution
                .decode("arguments", saved.arguments, arguments)
                .map_err(serde::de::Error::custom)?,
            environment: unreadable_execution
                .decode("environment", saved.environment, environment)
                .map_err(serde::de::Error::custom)?,
            unreadable_execution,
        })
    }
}

impl Serialize for AgentLaunchSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some(container) = &self.unreadable_execution.1
            && !self.container_repair_requested()
        {
            return container
                .value()
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer);
        }
        let executable = self
            .unreadable_execution
            .saved(
                "executable",
                self.executable.clone().map(toml::Value::String),
            )
            .map_err(serde::ser::Error::custom)?;
        let arguments =
            toml::Value::try_from(&self.arguments).map_err(serde::ser::Error::custom)?;
        let environment =
            toml::Value::try_from(&self.environment).map_err(serde::ser::Error::custom)?;
        SavedAgentLaunchSettings {
            model: self.model.clone(),
            effort: self.effort.clone(),
            permission_mode: self.permission_mode.clone(),
            executable,
            arguments: self
                .unreadable_execution
                .saved("arguments", Some(arguments))
                .map_err(serde::ser::Error::custom)?,
            environment: self
                .unreadable_execution
                .saved("environment", Some(environment))
                .map_err(serde::ser::Error::custom)?,
        }
        .serialize(serializer)
    }
}

pub fn provider_key(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Omp => "omp",
    }
}

/// Saved provider launch preferences, including unavailable pins that must
/// remain visible and must never silently fall back to another executable.
pub fn provider_settings(provider: Provider) -> Result<AgentLaunchSettings, String> {
    let path = super::default_path().map_err(|error| error.to_string())?;
    let settings = super::load(path).map_err(|error| error.to_string())?;
    Ok(settings
        .agents
        .get(provider_key(provider))
        .cloned()
        .unwrap_or_default())
}

/// Account discovery and authentication use the same pinned CLI as launches.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn provider_executable(
    provider: Provider,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<std::path::PathBuf, String> {
    provider_settings(provider)?.resolve_executable(provider, search_path)
}

pub fn permission_modes(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &[
            "manual",
            "acceptEdits",
            "auto",
            "plan",
            "dontAsk",
            "bypassPermissions",
        ],
        Provider::Codex => &["readOnly", "workspaceWrite", "yolo"],
        Provider::Omp => &[],
    }
}

/// Explicit CLI requests also accept provider approval policies that are not
/// conversation presets. Keep this distinct from the saved composer choices.
pub fn validate_permission_override(provider: Provider, mode: &str) -> Result<(), String> {
    let supported = permission_modes(provider).contains(&mode)
        || match provider {
            Provider::Claude => mode == "default",
            // Preserve approval-policy values understood by recorded Codex recipes,
            // including policies from older CLI versions.
            Provider::Codex => matches!(mode, "on-request" | "on-failure" | "untrusted" | "never"),
            Provider::Omp => matches!(mode, "always-ask" | "write" | "yolo"),
        };
    if supported {
        Ok(())
    } else {
        Err(format!(
            "Choose a permission mode offered by {}.",
            provider.label()
        ))
    }
}

/// Executable paths are absolute so a launch in another worktree means the
/// same thing. `access` checks the current user's actual execute permission.
pub fn executable_path_syntax(path: &str) -> bool {
    Path::new(path).is_absolute() && !path.contains('\0')
}

#[cfg(test)]
thread_local! {
    static EXECUTABLE_CHECKS_FORBIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Prove reducer paths cannot acquire filesystem metadata; worker threads
/// have their own guard state and still perform the real availability check.
#[cfg(test)]
pub(crate) fn without_executable_checks<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXECUTABLE_CHECKS_FORBIDDEN.set(self.0);
        }
    }
    let _restore = Restore(EXECUTABLE_CHECKS_FORBIDDEN.replace(true));
    f()
}

pub fn executable_available(path: &str) -> bool {
    #[cfg(test)]
    EXECUTABLE_CHECKS_FORBIDDEN.with(|forbidden| {
        assert!(
            !forbidden.get(),
            "filesystem availability check on reducer thread"
        )
    });
    let path = Path::new(path);
    path.is_absolute()
        && path.is_file()
        && CString::new(path.as_os_str().as_bytes()).is_ok_and(|path| {
            // SAFETY: this is a live, NUL-terminated path and access retains no pointer.
            unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
        })
}

pub fn environment_name_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn environment_reserved(provider: Provider, name: &str) -> bool {
    name.starts_with("CAGENTS_")
        || matches!(name, "CLAUDE_CONFIG_DIR" | "CODEX_HOME")
        || crate::account::credential_overrides(provider).contains(&name)
}

impl AgentLaunchSettings {
    pub(crate) fn unreadable_container(&self) -> Option<UnreadablePolicyReset> {
        self.unreadable_execution.1.clone()
    }

    pub(crate) fn unreadable_field(&self, field: &str) -> Option<UnreadablePolicyReset> {
        self.unreadable_execution
            .0
            .get(field)
            .map(|document| UnreadablePolicyReset {
                all_providers: false,
                field: Some(field.into()),
                document: document.clone(),
            })
    }

    pub(crate) fn repair_requested(&self, observed: &UnreadablePolicyReset) -> bool {
        self.unreadable_execution
            .2
            .contains(observed.field().unwrap_or("container"))
    }

    fn container_repair_requested(&self) -> bool {
        self.unreadable_execution.2.contains("container")
    }

    fn stored_unreadable_container(&self) -> Option<UnreadablePolicyReset> {
        (!self.container_repair_requested())
            .then(|| self.unreadable_container())
            .flatten()
    }

    fn stored_unreadable_field(&self, field: &str) -> Option<&String> {
        self.unreadable_execution
            .0
            .get(field)
            .filter(|_| !self.unreadable_execution.2.contains(field))
    }
    /// Conversation presets do not change the CLI used for account metadata.
    pub(crate) fn same_execution_policy(&self, other: &Self) -> bool {
        self.executable == other.executable
            && self.arguments == other.arguments
            && self.environment == other.environment
            && self.unreadable_execution == other.unreadable_execution
    }

    /// Replacing a control explicitly repairs only that execution leaf.
    /// Leaving its value blank is an explicit choice of the default too.
    pub fn set_executable(&mut self, executable: Option<String>) {
        if self.unreadable_execution.0.contains_key("executable") {
            self.unreadable_execution.2.insert("executable".into());
        }
        self.executable = executable;
    }

    pub fn set_arguments(&mut self, arguments: Vec<String>) {
        if self.unreadable_execution.0.contains_key("arguments") {
            self.unreadable_execution.2.insert("arguments".into());
        }
        self.arguments = arguments;
    }

    pub fn set_environment(&mut self, environment: BTreeMap<String, String>) {
        if self.unreadable_execution.0.contains_key("environment") {
            self.unreadable_execution.2.insert("environment".into());
        }
        self.environment = environment;
    }

    pub(crate) fn validate_readable_execution(&self, provider: Provider) -> Result<(), String> {
        if !self.unreadable_execution.2.is_empty() {
            return Err(format!(
                "{} preference repair is not acknowledged yet. Save the repair before launching or discovering this provider.",
                provider.label()
            ));
        }
        if let Some(container) = &self.unreadable_execution.1 {
            return Err(if container.all_providers {
                "Saved provider preferences could not be read. Reset the unreadable provider preferences in Agents settings before launching or discovering providers.".into()
            } else {
                format!(
                    "{} saved preferences could not be read. Reset this provider's unreadable preferences in Agents settings before launching or discovering it.",
                    provider.label()
                )
            });
        }
        if self.unreadable_execution.0.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} saved {} could not be read. Repair these fields in Agents settings before launching or discovering this provider.",
                provider.label(),
                self.unreadable_execution
                    .0
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    /// Resolve from this immutable policy capture so probes cannot pair an
    /// executable from one settings revision with arguments from another.
    /// A provider's explicit PATH applies to discovery as it does to launch.
    #[cfg(any(feature = "ui", feature = "host"))]
    pub fn resolve_executable(
        &self,
        provider: Provider,
        search_path: Option<&std::ffi::OsStr>,
    ) -> Result<std::path::PathBuf, String> {
        self.validate_readable_execution(provider)?;
        if let Some(executable) = &self.executable {
            if !executable_available(executable) {
                return Err(format!(
                    "{} executable must be an absolute path to an executable file.",
                    provider.label()
                ));
            }
            return Ok(executable.into());
        }
        self.validate_provider_path()?;
        let search_path = self
            .environment
            .get("PATH")
            .map(|path| std::ffi::OsStr::new(path))
            .or(search_path);
        crate::account::probe::find_executable(provider.executable(), search_path)
            .ok_or_else(|| format!("{} is not installed", provider.label()))
    }

    /// Authentication and discovery consume execution policy without model,
    /// effort or permission defaults, which are conversation choices.
    pub fn validate_execution(&self, provider: Provider) -> Result<(), String> {
        self.validate_execution_fields(provider)?;
        self.validate_provider_path()
    }

    // A pin fixes program identity even when a provider needs relative PATH
    // entries for its own tools. Without one, discovery cannot resolve a
    // project-relative entry without knowing the eventual launch directory.
    fn validate_provider_path(&self) -> Result<(), String> {
        if self.executable.is_none()
            && self.environment.get("PATH").is_some_and(|path| {
                std::env::split_paths(path).any(|directory| !directory.is_absolute())
            })
        {
            return Err("Set a provider executable or use only absolute directories in PATH; empty and relative entries depend on the launch directory.".into());
        }
        Ok(())
    }

    fn validate_execution_fields(&self, provider: Provider) -> Result<(), String> {
        self.validate_execution_fields_with_availability(provider, true)
    }

    fn validate_execution_fields_with_availability(
        &self,
        provider: Provider,
        availability: bool,
    ) -> Result<(), String> {
        self.validate_readable_execution(provider)?;
        if self.executable.as_deref().is_some_and(|path| {
            if availability {
                !executable_available(path)
            } else {
                !executable_path_syntax(path)
            }
        }) {
            return Err(format!(
                "{} executable must be an absolute path to an executable file.",
                provider.label()
            ));
        }
        for (name, value) in &self.environment {
            if !environment_name_valid(name) {
                return Err("Environment names must start with a letter or underscore and contain only letters, numbers and underscores.".into());
            }
            if environment_reserved(provider, name) {
                return Err(format!(
                    "{name} is reserved for account isolation and cannot be overridden."
                ));
            }
            if value.contains('\0') {
                return Err("Environment values cannot contain a NUL character.".into());
            }
        }
        if self
            .arguments
            .iter()
            .any(|argument| argument.contains('\0'))
        {
            return Err("Arguments cannot contain a NUL character.".into());
        }
        Ok(())
    }

    /// Validate the recorded policy's syntax without probing a filesystem.
    /// Availability is checked by save/launch workers on the owning device.
    pub fn validate_syntax(&self, provider: Provider) -> Result<(), String> {
        self.validate_fields_with_availability(provider, false)?;
        if self
            .executable
            .as_deref()
            .is_some_and(|path| !executable_path_syntax(path))
        {
            return Err(format!(
                "{} executable must be an absolute path to an executable file.",
                provider.label()
            ));
        }
        self.validate_provider_path()
    }

    pub fn validate(&self, provider: Provider) -> Result<(), String> {
        self.validate_fields(provider)?;
        self.validate_provider_path()
    }

    fn validate_fields(&self, provider: Provider) -> Result<(), String> {
        self.validate_fields_with_availability(provider, true)
    }

    fn validate_fields_with_availability(
        &self,
        provider: Provider,
        availability: bool,
    ) -> Result<(), String> {
        self.validate_execution_fields_with_availability(provider, availability)?;
        for value in self.model.iter().chain(self.effort.iter()) {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                return Err(
                    "Model and effort must be non-empty names without control characters.".into(),
                );
            }
        }
        if self
            .permission_mode
            .as_deref()
            .is_some_and(|mode| !permission_modes(provider).contains(&mode))
        {
            return Err(format!(
                "Choose a permission mode offered by {}.",
                provider.label()
            ));
        }
        Ok(())
    }

    /// Validate changed fields independently so retained invalid siblings can
    /// be repaired one control at a time without weakening launch validation.
    pub fn validate_changes(&self, previous: &Self, provider: Provider) -> Result<(), String> {
        self.validate_changes_with_availability(previous, provider, true)
    }

    pub fn validate_changes_syntax(
        &self,
        previous: &Self,
        provider: Provider,
    ) -> Result<(), String> {
        self.validate_changes_with_availability(previous, provider, false)
    }

    fn validate_changes_with_availability(
        &self,
        previous: &Self,
        provider: Provider,
        availability: bool,
    ) -> Result<(), String> {
        if self.unreadable_execution.1.is_some() && self != previous {
            let mut requested = previous.clone();
            requested.unreadable_execution.2.insert("container".into());
            if self != &requested {
                return self.validate_readable_execution(provider);
            }
        }
        Self {
            model: (self.model != previous.model)
                .then(|| self.model.clone())
                .flatten(),
            effort: (self.effort != previous.effort)
                .then(|| self.effort.clone())
                .flatten(),
            permission_mode: (self.permission_mode != previous.permission_mode)
                .then(|| self.permission_mode.clone())
                .flatten(),
            executable: (self.executable != previous.executable)
                .then(|| self.executable.clone())
                .flatten(),
            arguments: if self.arguments != previous.arguments {
                self.arguments.clone()
            } else {
                vec![]
            },
            environment: if self.environment != previous.environment {
                self.environment.clone()
            } else {
                Default::default()
            },
            ..Default::default()
        }
        .validate_fields_with_availability(provider, availability)?;
        if self.executable != previous.executable
            || self.environment.get("PATH") != previous.environment.get("PATH")
        {
            // Use the effective pin, including an unchanged pin, rather than
            // the field-only validation projection above. Unchanged invalid
            // execution policy must not prevent unrelated repairs.
            self.validate_provider_path()?;
        }
        Ok(())
    }

    /// Apply before account isolation. The selected home and credential
    /// removals always have the final say over inherited launch environment.
    pub fn apply_launch(&self, recipe: &mut LaunchRecipe) {
        if let Some(executable) = &self.executable {
            recipe.program.clone_from(executable);
        }
        recipe.arguments.extend(self.arguments.iter().cloned());
        recipe.environment.extend(self.environment.clone());
    }

    pub fn append_options(
        &self,
        provider: Provider,
        arguments: &mut Vec<String>,
        overrides: LaunchDefaults<'_>,
    ) {
        if let Some(model) = overrides.model.or(self.model.as_deref()) {
            set_option(arguments, "--model", model);
        }
        if let Some(effort) = overrides.effort.or(self.effort.as_deref()) {
            match provider {
                Provider::Claude => set_option(arguments, "--effort", effort),
                Provider::Omp => set_option(arguments, "--thinking", effort),
                Provider::Codex => {
                    let key = crate::chat::CODEX_REASONING_EFFORT_KEY;
                    remove_config_key(arguments, key);
                    arguments.extend(["--config".into(), format!("{key}={effort}")]);
                }
            }
        }
        if let Some(mode) = overrides
            .permission_mode
            .or(self.permission_mode.as_deref())
        {
            match provider {
                Provider::Claude => {
                    arguments.retain(|argument| argument != "--dangerously-skip-permissions");
                    set_option(
                        arguments,
                        "--permission-mode",
                        if mode == "manual" { "default" } else { mode },
                    );
                }
                Provider::Codex => {
                    arguments.retain(|argument| {
                        !matches!(
                            argument.as_str(),
                            "--yolo" | "--dangerously-bypass-approvals-and-sandbox"
                        )
                    });
                    if mode == "yolo" {
                        arguments.push("--dangerously-bypass-approvals-and-sandbox".into());
                    } else if matches!(mode, "readOnly" | "workspaceWrite") {
                        set_option(
                            arguments,
                            "--sandbox",
                            if mode == "readOnly" {
                                "read-only"
                            } else {
                                "workspace-write"
                            },
                        );
                        set_option(arguments, "--ask-for-approval", "on-request");
                    } else {
                        set_option(arguments, "--ask-for-approval", mode);
                    }
                }
                Provider::Omp => set_option(arguments, "--approval-mode", mode),
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct LaunchDefaults<'a> {
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub permission_mode: Option<&'a str>,
}

fn remove_option(arguments: &mut Vec<String>, flag: &str) {
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == flag {
            arguments.remove(index);
            if index < arguments.len() {
                arguments.remove(index);
            }
        } else if arguments[index]
            .strip_prefix(flag)
            .is_some_and(|suffix| suffix.starts_with('='))
        {
            arguments.remove(index);
        } else {
            index += 1;
        }
    }
}

fn set_option(arguments: &mut Vec<String>, flag: &str, value: &str) {
    remove_option(arguments, flag);
    arguments.extend([flag.into(), value.into()]);
}

/// Record the composer/provider-confirmed selection in the launch recipe.
/// None deliberately clears a configured default when the user chooses Automatic.
pub fn set_model_options(
    arguments: &mut Vec<String>,
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
) {
    remove_option(arguments, "--model");
    match provider {
        Provider::Claude => remove_option(arguments, "--effort"),
        Provider::Omp => remove_option(arguments, "--thinking"),
        Provider::Codex => remove_config_key(arguments, crate::chat::CODEX_REASONING_EFFORT_KEY),
    }
    AgentLaunchSettings::default().append_options(
        provider,
        arguments,
        LaunchDefaults {
            model,
            effort,
            permission_mode: None,
        },
    );
}

fn remove_config_key(arguments: &mut Vec<String>, key: &str) {
    let matches_key = |value: &str| {
        value
            .split_once('=')
            .is_some_and(|(name, _)| name.trim() == key)
    };
    let mut index = 0;
    while index < arguments.len() {
        if matches!(arguments[index].as_str(), "-c" | "--config")
            && arguments
                .get(index + 1)
                .is_some_and(|value| matches_key(value))
        {
            arguments.drain(index..index + 2);
        } else if arguments[index]
            .strip_prefix("--config=")
            .is_some_and(matches_key)
        {
            arguments.remove(index);
        } else {
            index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn malformed_policy_document(policy: &str) -> String {
        format!("schema_version = 3\n[agents.claude]\n{policy}\n")
    }

    #[test]
    fn unreadable_containers_preserve_scope_and_require_observed_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for all in [false, true] {
            for raw in [
                "\"private-raw-policy\"",
                "[]",
                "7",
                "true",
                "nan",
                "1979-05-27T07:32:00Z",
            ] {
                let original = if all {
                    format!("schema_version = 3\nagents = {raw}\n")
                } else {
                    format!(
                        "schema_version = 3\n[agents]\nclaude = {raw}\n[agents.codex]\nexecutable = \"/bin/sh\"\narguments = [\"--sibling\"]\nenvironment = {{ KEEP = \"sibling-value\" }}\n"
                    )
                };
                std::fs::write(&path, &original).unwrap();
                let loaded = super::super::load(&path).unwrap();
                let observed = loaded.agents["claude"].unreadable_container().unwrap();
                assert_eq!(observed.all_providers(), all);
                assert!(!format!("{observed:?}").contains("private-raw-policy"));
                for provider in Provider::ALL {
                    let configuration = loaded
                        .agents
                        .get(provider_key(provider))
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(
                        configuration.validate_execution(provider).is_err(),
                        all || provider == Provider::Claude
                    );
                }
                assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
                let mut unrelated = loaded.clone();
                unrelated.terminal_font_size = 21.0;
                super::super::save_merged(&path, &loaded, &unrelated).unwrap();
                let retained = super::super::load(&path).unwrap();
                assert_eq!(retained.agents, loaded.agents);
                let mut requested = retained.clone();
                assert!(repair_unreadable_policy(
                    &mut requested.agents,
                    Provider::Claude,
                    &observed
                ));
                assert!(
                    requested.agents["claude"]
                        .validate_execution(Provider::Claude)
                        .is_err()
                );
                super::super::save_merged(&path, &retained, &requested).unwrap();
                let repaired = super::super::load(&path).unwrap();
                assert_eq!(repaired.terminal_font_size, 21.0);
                if all {
                    assert!(repaired.agents.is_empty());
                } else {
                    assert_eq!(repaired.agents["codex"], loaded.agents["codex"]);
                    repaired.agents["claude"]
                        .validate_execution(Provider::Claude)
                        .unwrap();
                }
                // The request remains a refusal until the durable receipt is
                // acknowledged, even though its canonical bytes now exist.
                assert!(
                    requested.agents["claude"]
                        .validate_execution(Provider::Claude)
                        .is_err()
                );
                confirm_policy_repairs(&requested.agents, &repaired.agents).unwrap();
                complete_policy_repairs(&mut requested.agents);
                assert_eq!(requested.agents, repaired.agents);
                assert!(!repair_unreadable_policy(
                    &mut requested.agents,
                    Provider::Claude,
                    &observed
                ));
                // The same captured reset cannot erase a newer valid policy.
                let replacement = "schema_version = 3\n[agents.claude]\nexecutable = \"/bin/sh\"\narguments = [\"--new-owner\"]\n";
                std::fs::write(&path, replacement).unwrap();
                let mut stale_reset = loaded.clone();
                assert!(repair_unreadable_policy(
                    &mut stale_reset.agents,
                    Provider::Claude,
                    &observed
                ));
                assert!(super::super::save_merged(&path, &loaded, &stale_reset).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), replacement.as_bytes());
                let mut newer = super::super::load(&path).unwrap();
                assert!(!repair_unreadable_policy(
                    &mut newer.agents,
                    Provider::Claude,
                    &observed
                ));
                // Nor can an ordinary old field edit erase a container it
                // never observed while it was still a valid table.
                let valid_base = newer.clone();
                newer
                    .agents
                    .get_mut("claude")
                    .unwrap()
                    .arguments
                    .push("--ordinary-edit".into());
                std::fs::write(&path, &original).unwrap();
                assert!(super::super::save_merged(&path, &valid_base, &newer).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
            }
        }
    }

    #[test]
    fn unreadable_execution_leaves_survive_merges_until_each_explicit_repair() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let original = malformed_policy_document(
            r#"model = "kept-model"
permission_mode = "plan"
executable = ["/pinned/wrapper"]
arguments = ["--prefix", 7, "--suffix"]
environment = { KEEP = "kept-value", BROKEN = 7 }"#,
        );
        std::fs::write(&path, &original).unwrap();
        let loaded = super::super::load(&path).unwrap();
        let bad = &loaded.agents["claude"];
        assert!(bad.validate_execution(Provider::Claude).is_err());
        assert!(bad.validate_syntax(Provider::Claude).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        assert_eq!(bad.model.as_deref(), Some("kept-model"));
        assert_eq!(bad.permission_mode.as_deref(), Some("plan"));
        assert_eq!(bad, &bad.clone());
        assert!(!format!("{bad:?}").contains("kept-value"));
        assert!(!format!("{bad:?}").contains("/pinned/wrapper"));
        assert_eq!(
            serde_json::from_str::<AgentLaunchSettings>(&serde_json::to_string(bad).unwrap())
                .unwrap(),
            *bad
        );
        let raw = toml::from_str::<toml::Value>(&original).unwrap();
        let mut unrelated = loaded.clone();
        unrelated.terminal_font_size = 21.0;
        super::super::save_merged(&path, &loaded, &unrelated).unwrap();
        let saved =
            toml::from_str::<toml::Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["agents"], raw["agents"]);
        let mut current = super::super::load(&path).unwrap();
        let stale = current.clone();
        let mut repaired = current.clone();
        repaired
            .agents
            .get_mut("claude")
            .unwrap()
            .set_executable(None);
        assert!(!repaired.agents["claude"].same_execution_policy(&current.agents["claude"]));
        assert!(
            repaired.agents["claude"]
                .validate_execution(Provider::Claude)
                .is_err()
        );
        super::super::save_merged(&path, &current, &repaired).unwrap();
        current = super::super::load(&path).unwrap();
        assert!(
            !current.agents["claude"]
                .unreadable_execution
                .0
                .contains_key("executable")
        );
        assert_eq!(current.agents["claude"].unreadable_execution.0.len(), 2);
        // An older window's unrelated edit must not restore the repaired pin.
        let mut stale_edit = stale.clone();
        stale_edit.terminal_font_size = 22.0;
        super::super::save_merged(&path, &stale, &stale_edit).unwrap();
        assert_eq!(super::super::load(&path).unwrap().agents, current.agents);
        repaired = current.clone();
        let observed = repaired.agents["claude"]
            .unreadable_field("arguments")
            .unwrap();
        assert!(repair_unreadable_policy(
            &mut repaired.agents,
            Provider::Claude,
            &observed
        ));
        super::super::save_merged(&path, &current, &repaired).unwrap();
        current = super::super::load(&path).unwrap();
        assert_eq!(current.agents["claude"].unreadable_execution.0.len(), 1);
        assert!(
            current.agents["claude"]
                .validate_execution(Provider::Claude)
                .is_err()
        );
        repaired = current.clone();
        let observed = repaired.agents["claude"]
            .unreadable_field("environment")
            .unwrap();
        assert!(repair_unreadable_policy(
            &mut repaired.agents,
            Provider::Claude,
            &observed
        ));
        super::super::save_merged(&path, &current, &repaired).unwrap();
        current = super::super::load(&path).unwrap();
        current.agents["claude"]
            .validate_execution(Provider::Claude)
            .unwrap();
        assert_eq!(
            current.agents["claude"].model.as_deref(),
            Some("kept-model")
        );
        assert_eq!(
            current.agents["claude"].permission_mode.as_deref(),
            Some("plan")
        );
        assert!(current.agents["claude"].executable.is_none());
        assert_eq!(current.agents["claude"].arguments, ["--prefix", "--suffix"]);
        assert_eq!(
            current.agents["claude"].environment,
            BTreeMap::from([("KEEP".into(), "kept-value".into())])
        );
        assert_eq!(current.terminal_font_size, 22.0);
        let mut invalid = current.clone();
        invalid.ui_scale = 999.0;
        let before = std::fs::read(&path).unwrap();
        assert!(super::super::save_merged(&path, &current, &invalid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unreadable_execution_keeps_nonexecution_fallback_and_nan_equality() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for policy in [
            "model = [\"unreadable-model\"]\neffort = 7\narguments = [\"--kept\"]\nenvironment = { KEPT = \"value\" }",
            "model = [\"unreadable-model\"]\neffort = 7\narguments = nan\nenvironment = { KEPT = \"value\" }",
        ] {
            let original = malformed_policy_document(policy);
            std::fs::write(&path, &original).unwrap();
            let detailed = super::super::load_detailed(&path).unwrap();
            let configuration = &detailed.settings.agents["claude"];
            assert!(configuration.model.is_none());
            assert!(configuration.effort.is_none());
            assert_eq!(configuration.environment["KEPT"], "value");
            assert_eq!(configuration, &configuration.clone());
            assert_eq!(detailed.set_aside().count(), 2);
            if policy.contains("arguments = nan") {
                assert!(configuration.validate_execution(Provider::Claude).is_err());
                let serialized = toml::to_string(configuration).unwrap();
                assert_eq!(
                    toml::from_str::<AgentLaunchSettings>(&serialized).unwrap(),
                    *configuration
                );
            } else {
                assert_eq!(configuration.arguments, ["--kept"]);
                configuration.validate_execution(Provider::Claude).unwrap();
            }
            assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        }
    }

    #[test]
    fn policy_repair_acknowledgments_match_raw_intent_and_persisted_scope() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        std::fs::write(
            &path,
            malformed_policy_document(
                "arguments = [\"--before\", 7, \"--after\"]\nenvironment = { KEEP = \"value\" }",
            ),
        )
        .unwrap();
        let original = super::super::load(&path).unwrap();
        let observed = original.agents["claude"]
            .unreadable_field("arguments")
            .unwrap();
        let mut requested = original.clone();
        assert!(repair_unreadable_policy(
            &mut requested.agents,
            Provider::Claude,
            &observed
        ));
        assert!(confirm_policy_repairs(&requested.agents, &original.agents).is_err());
        super::super::save_merged(&path, &original, &requested).unwrap();
        let persisted = super::super::load(&path).unwrap();
        confirm_policy_repairs(&requested.agents, &persisted.agents).unwrap();
        assert!(
            requested.agents["claude"]
                .validate_execution(Provider::Claude)
                .is_err()
        );
        let mut wrong_persisted = persisted.clone();
        wrong_persisted.agents.get_mut("claude").unwrap().arguments = vec!["--different".into()];
        assert!(confirm_policy_repairs(&requested.agents, &wrong_persisted.agents).is_err());
        let mut newer_intent = requested.clone();
        newer_intent
            .agents
            .get_mut("claude")
            .unwrap()
            .set_arguments(vec!["--new-intent".into()]);
        let before = newer_intent.clone();
        acknowledge_policy_repairs(&mut newer_intent.agents, &requested.agents);
        assert_eq!(newer_intent, before);
        std::fs::write(
            &path,
            malformed_policy_document("arguments = [false, \"--new-raw\"]"),
        )
        .unwrap();
        let mut newer_raw = super::super::load(&path).unwrap();
        let next_observed = newer_raw.agents["claude"]
            .unreadable_field("arguments")
            .unwrap();
        assert!(!repair_unreadable_policy(
            &mut newer_raw.agents,
            Provider::Claude,
            &observed
        ));
        assert!(repair_unreadable_policy(
            &mut newer_raw.agents,
            Provider::Claude,
            &next_observed
        ));
        let before = newer_raw.clone();
        acknowledge_policy_repairs(&mut newer_raw.agents, &requested.agents);
        assert_eq!(newer_raw, before);
        let mut current = requested.clone();
        current.agents.get_mut("claude").unwrap().model = Some("later-model".into());
        acknowledge_policy_repairs(&mut current.agents, &requested.agents);
        current.agents["claude"]
            .validate_execution(Provider::Claude)
            .unwrap();
        assert_eq!(
            current.agents["claude"].model.as_deref(),
            Some("later-model")
        );
        assert_eq!(current.agents["claude"].arguments, ["--before", "--after"]);
        assert_eq!(current.agents["claude"].environment["KEEP"], "value");
    }

    #[test]
    fn canonical_policy_repair_retries_after_confirmation_failure_without_overwriting_new_policy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for (index, original) in [
            "schema_version = 3\n[agents.claude]\narguments = [\"--before\", 7, \"--after\"]\nenvironment = { KEEP = \"value\" }\n",
            "schema_version = 3\n[agents]\nclaude = [\"private-policy\"]\n[agents.codex]\narguments = [\"--sibling\"]\n",
            "schema_version = 3\nagents = [\"private-policy\"]\n",
        ].into_iter().enumerate() {
            std::fs::write(&path, original).unwrap();
            let base = super::super::load(&path).unwrap();
            let observed = base.agents["claude"].unreadable_container()
                .or_else(|| base.agents["claude"].unreadable_field("arguments")).unwrap();
            let mut requested = base.clone();
            assert!(repair_unreadable_policy(&mut requested.agents, Provider::Claude, &observed));
            let canonical = directory.path().join(format!("canonical-{index}.toml"));
            let backup = canonical.clone();
            let result = super::super::with_agent_repair_confirmation_hook(move |path| {
                // The actual canonical write completed. Make the real
                // load_unlocked confirmation fail on a non-file destination.
                std::fs::rename(path, &backup).unwrap();
                std::fs::create_dir(path).unwrap();
            }, || super::super::save_merged(&path, &base, &requested));
            assert!(result.is_err());
            assert!(path.is_dir());
            assert!(requested.agents["claude"].validate_execution(Provider::Claude).is_err());
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&canonical, &path).unwrap();
            let persisted = super::super::load(&path).unwrap();
            confirm_policy_repairs(&requested.agents, &persisted.agents).unwrap();
            // Preserve another window's unrelated edit during exact retry.
            let mut other = persisted.clone();
            other.terminal_font_size = 31.0;
            super::super::save_merged(&path, &persisted, &other).unwrap();
            super::super::save_merged(&path, &base, &requested).unwrap();
            let retried = super::super::load(&path).unwrap();
            assert_eq!(retried.terminal_font_size, 31.0);
            assert_eq!(retried.agents, persisted.agents);
            assert!(requested.agents["claude"].validate_execution(Provider::Claude).is_err());
            complete_policy_repairs(&mut requested.agents);
            assert_eq!(requested.agents, retried.agents);
            // A different canonical policy is never replaced by retrying the
            // old intent, including an already-confirmed lane's copied base.
            let mut changed = retried.clone();
            changed.agents.entry("claude".into()).or_default().arguments = vec!["--new-policy".into()];
            super::super::save_merged(&path, &retried, &changed).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let mut old_request = base.clone();
            assert!(repair_unreadable_policy(&mut old_request.agents, Provider::Claude, &observed));
            assert!(super::super::save_merged(&path, &base, &old_request).is_err());
            assert!(super::super::save_merged(&path, &old_request, &old_request).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn unchanged_nan_policy_cannot_undo_another_windows_repair() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        for original in [
            "schema_version = 3\n[agents.claude]\narguments = nan\n",
            "schema_version = 3\n[agents]\nclaude = nan\n",
            "schema_version = 3\nagents = nan\n",
        ] {
            std::fs::write(&path, original).unwrap();
            let first = super::super::load(&path).unwrap();
            let stale = super::super::load(&path).unwrap();
            assert_eq!(first.agents, stale.agents);
            let observed = first.agents["claude"]
                .unreadable_container()
                .or_else(|| first.agents["claude"].unreadable_field("arguments"))
                .unwrap();
            let mut repair = first.clone();
            assert!(repair_unreadable_policy(
                &mut repair.agents,
                Provider::Claude,
                &observed
            ));
            super::super::save_merged(&path, &first, &repair).unwrap();
            let repaired = super::super::load(&path).unwrap();
            let mut other = stale.clone();
            other.terminal_font_size = 27.0;
            super::super::save_merged(&path, &stale, &other).unwrap();
            let final_settings = super::super::load(&path).unwrap();
            assert_eq!(final_settings.terminal_font_size, 27.0);
            assert_eq!(final_settings.agents, repaired.agents);
            for provider in Provider::ALL {
                final_settings
                    .agents
                    .get(provider_key(provider))
                    .cloned()
                    .unwrap_or_default()
                    .validate_execution(provider)
                    .unwrap();
            }
        }
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn unreadable_saved_policy_refuses_launch_and_every_discovery_entrypoint() {
        const MARKER: &str = "CAGENTS_TEST_UNREADABLE_PROVIDER_POLICY_ROOT";
        let root = if let Some(root) = std::env::var_os(MARKER) {
            std::path::PathBuf::from(root)
        } else {
            let directory = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "settings::agents::tests::unreadable_saved_policy_refuses_launch_and_every_discovery_entrypoint", "--nocapture", "--test-threads=1"])
                .env(MARKER, directory.path())
                .env("XDG_CONFIG_HOME", directory.path().join("config"))
                .env("XDG_STATE_HOME", directory.path().join("state"))
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("XDG_CACHE_HOME", directory.path().join("cache"))
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        };
        let binaries = root.join("bin");
        std::fs::create_dir(&binaries).unwrap();
        for provider in Provider::ALL {
            let binary = binaries.join(provider.executable());
            std::fs::write(&binary, "#!/bin/sh\nexit 91\n").unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = super::super::default_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for provider in Provider::ALL {
            let binary = binaries.join(provider.executable());
            for pin in [None, Some(binary.to_str().unwrap())] {
                let valid = AgentLaunchSettings {
                    executable: pin.map(str::to_owned),
                    arguments: vec!["--kept".into()],
                    environment: BTreeMap::from([("KEPT".into(), "value".into())]),
                    ..Default::default()
                };
                assert_eq!(
                    valid
                        .resolve_executable(provider, Some(binaries.as_os_str()))
                        .unwrap(),
                    binary
                );
                assert!(
                    crate::account::probe::Cli::configured(
                        provider,
                        Some(binaries.as_os_str()),
                        valid.clone()
                    )
                    .is_ok()
                );
                let valid_value = toml::Value::try_from(&valid).unwrap();
                for (field, bad_value) in [
                    (
                        "executable",
                        toml::Value::Array(vec![toml::Value::String("/pinned/wrapper".into())]),
                    ),
                    ("arguments", toml::Value::String("--unreadable".into())),
                    (
                        "arguments",
                        toml::Value::Array(vec![
                            toml::Value::String("--kept".into()),
                            toml::Value::Integer(7),
                        ]),
                    ),
                    ("environment", toml::Value::Array(vec![])),
                    (
                        "environment",
                        toml::Value::Table(toml::Table::from_iter([(
                            "KEPT".into(),
                            toml::Value::Integer(7),
                        )])),
                    ),
                ] {
                    let mut bad_value_table = valid_value.as_table().unwrap().clone();
                    bad_value_table.insert(field.into(), bad_value);
                    let document = toml::to_string(&toml::Table::from_iter([
                        ("schema_version".into(), toml::Value::Integer(3)),
                        (
                            "agents".into(),
                            toml::Value::Table(toml::Table::from_iter([(
                                provider_key(provider).into(),
                                toml::Value::Table(bad_value_table),
                            )])),
                        ),
                    ]))
                    .unwrap();
                    std::fs::write(&path, &document).unwrap();
                    let loaded = super::super::load(&path).unwrap();
                    let configuration = &loaded.agents[provider_key(provider)];
                    assert!(
                        configuration.validate_execution(provider).is_err(),
                        "{provider:?}/{field}"
                    );
                    assert!(
                        configuration
                            .resolve_executable(provider, Some(binaries.as_os_str()))
                            .is_err()
                    );
                    assert!(provider_executable(provider, Some(binaries.as_os_str())).is_err());
                    assert!(
                        crate::account::probe::Cli::discover(provider, Some(binaries.as_os_str()))
                            .is_err()
                    );
                    assert!(
                        crate::account::probe::Cli::configured(
                            provider,
                            Some(binaries.as_os_str()),
                            configuration.clone()
                        )
                        .is_err()
                    );
                    assert!(
                        crate::chat::fresh_session_configured(
                            provider,
                            crate::model::TerminalId::new(),
                            root.clone(),
                            &crate::account::Account::system(provider),
                            configuration,
                            Default::default()
                        )
                        .is_err()
                    );
                    for sibling in Provider::ALL
                        .into_iter()
                        .filter(|candidate| *candidate != provider)
                    {
                        assert_eq!(
                            provider_executable(sibling, Some(binaries.as_os_str())).unwrap(),
                            binaries.join(sibling.executable())
                        );
                    }
                    assert_eq!(std::fs::read(&path).unwrap(), document.as_bytes());
                }
            }
        }
        for all in [false, true] {
            let document = if all {
                "schema_version = 3\nagents = [\"private-policy\"]\n"
            } else {
                "schema_version = 3\n[agents]\nclaude = [\"private-policy\"]\n"
            };
            std::fs::write(&path, document).unwrap();
            let loaded = super::super::load(&path).unwrap();
            for provider in Provider::ALL {
                let blocked = all || provider == Provider::Claude;
                let configuration = loaded
                    .agents
                    .get(provider_key(provider))
                    .cloned()
                    .unwrap_or_default();
                assert_eq!(
                    configuration
                        .resolve_executable(provider, Some(binaries.as_os_str()))
                        .is_err(),
                    blocked
                );
                assert_eq!(
                    provider_executable(provider, Some(binaries.as_os_str())).is_err(),
                    blocked
                );
                assert_eq!(
                    crate::account::probe::Cli::discover(provider, Some(binaries.as_os_str()))
                        .is_err(),
                    blocked
                );
                if blocked {
                    assert!(
                        crate::chat::fresh_session_configured(
                            provider,
                            crate::model::TerminalId::new(),
                            root.clone(),
                            &crate::account::Account::system(provider),
                            &configuration,
                            Default::default()
                        )
                        .is_err()
                    );
                }
            }
            assert_eq!(std::fs::read(&path).unwrap(), document.as_bytes());
        }
        let missing = AgentLaunchSettings {
            executable: Some(root.join("missing-pin").to_str().unwrap().into()),
            ..Default::default()
        };
        assert!(
            missing
                .resolve_executable(Provider::Claude, Some(binaries.as_os_str()))
                .is_err()
        );
    }

    #[test]
    fn ambiguous_provider_path_requires_a_pin_without_blocking_independent_repairs() {
        let previous = AgentLaunchSettings {
            environment: BTreeMap::from([("PATH".into(), "tools:/usr/bin".into())]),
            ..Default::default()
        };
        assert!(
            previous
                .validate_execution(Provider::Claude)
                .unwrap_err()
                .contains("absolute")
        );
        for path in ["", ":/usr/bin", "/usr/bin:", "/usr/bin:tools", "."] {
            let mut ambiguous = previous.clone();
            ambiguous.environment.insert("PATH".into(), path.into());
            assert!(ambiguous.validate(Provider::Claude).is_err(), "{path:?}");
        }
        let mut repaired = previous.clone();
        repaired.model = Some("fixture-model".into());
        repaired
            .validate_changes(&previous, Provider::Claude)
            .unwrap();
        repaired
            .environment
            .insert("FIXTURE_TOOL".into(), "value".into());
        repaired
            .validate_changes(&previous, Provider::Claude)
            .unwrap();
        repaired.executable = Some("/bin/sh".into());
        repaired
            .validate_changes(&previous, Provider::Claude)
            .unwrap();
        repaired.validate_execution(Provider::Claude).unwrap();
        let pinned = repaired.clone();
        repaired
            .environment
            .insert("PATH".into(), "relative-tools".into());
        repaired
            .validate_changes(&pinned, Provider::Claude)
            .unwrap();
        let pinned = repaired.clone();
        repaired.executable = None;
        assert!(
            repaired
                .validate_changes(&pinned, Provider::Claude)
                .is_err()
        );
        repaired
            .environment
            .insert("PATH".into(), "/usr/bin".into());
        repaired
            .validate_changes(&pinned, Provider::Claude)
            .unwrap();
        repaired.validate_execution(Provider::Claude).unwrap();

        let missing_pin = AgentLaunchSettings {
            executable: Some("/missing/provider".into()),
            ..previous
        };
        let mut repair_environment = missing_pin.clone();
        repair_environment
            .environment
            .insert("PATH".into(), "other-tools".into());
        repair_environment
            .validate_changes(&missing_pin, Provider::Claude)
            .unwrap();
        assert!(
            repair_environment
                .validate_execution(Provider::Claude)
                .is_err()
        );
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn relative_provider_path_is_refused_before_launch_or_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("host");
        let tools = directory.path().join("tools");
        for (root, marker) in [(&host, "host"), (&tools, "project")] {
            std::fs::create_dir(root).unwrap();
            let binary = root.join(Provider::Claude.executable());
            std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{marker}'\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = format!("tools:{}", host.display());
        let mut configuration = AgentLaunchSettings {
            environment: BTreeMap::from([("PATH".into(), path.clone())]),
            ..Default::default()
        };
        // This is the mismatch that execution policy must refuse: launching
        // a bare name sees the project CLI while discovery skips relative PATH.
        let output = std::process::Command::new(Provider::Claude.executable())
            .current_dir(directory.path())
            .env_clear()
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"project\n");
        assert_eq!(
            crate::account::probe::find_executable(
                Provider::Claude.executable(),
                Some(std::ffi::OsStr::new(&path))
            )
            .unwrap(),
            host.join(Provider::Claude.executable())
        );
        assert!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .is_err()
        );
        assert!(
            crate::chat::fresh_session_configured(
                Provider::Claude,
                crate::model::TerminalId::new(),
                directory.path().into(),
                &crate::account::Account::system(Provider::Claude),
                &configuration,
                LaunchDefaults::default(),
            )
            .is_err()
        );
        configuration.executable = Some(
            tools
                .join(Provider::Claude.executable())
                .to_str()
                .unwrap()
                .into(),
        );
        configuration.validate_execution(Provider::Claude).unwrap();
        assert_eq!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .unwrap(),
            tools.join(Provider::Claude.executable())
        );
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn provider_path_selects_the_same_binary_for_launch_and_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("host");
        let configured = directory.path().join("configured");
        let host_program = host.join(Provider::Claude.executable());
        let configured_program = configured.join(Provider::Claude.executable());
        for (root, marker) in [(&host, "host"), (&configured, "configured")] {
            std::fs::create_dir(root).unwrap();
            let binary = root.join(Provider::Claude.executable());
            std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{marker}'\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut configuration = AgentLaunchSettings::default();
        assert_eq!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .unwrap(),
            host_program
        );
        configuration
            .environment
            .insert("PATH".into(), configured.to_str().unwrap().into());
        let discovered = configuration
            .resolve_executable(Provider::Claude, Some(host.as_os_str()))
            .unwrap();
        assert_eq!(discovered, configured_program);
        let mut recipe = LaunchRecipe {
            program: Provider::Claude.executable().into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        configuration.apply_launch(&mut recipe);
        for program in [Path::new(&recipe.program), discovered.as_path()] {
            let output = std::process::Command::new(program)
                .args(&recipe.arguments)
                .current_dir(&recipe.working_directory)
                .env_clear()
                .envs(&recipe.environment)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"configured\n");
        }
        configuration.executable = Some(host_program.to_str().unwrap().into());
        assert_eq!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .unwrap(),
            host_program
        );
        configuration.executable = Some(directory.path().join("missing").to_str().unwrap().into());
        assert!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .is_err()
        );
        configuration.executable = None;
        configuration
            .environment
            .insert("PATH".into(), String::new());
        assert!(
            configuration
                .resolve_executable(Provider::Claude, Some(host.as_os_str()))
                .is_err()
        );
    }

    #[test]
    fn provider_tables_round_trip_and_reject_unsafe_launch_values() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("provider-wrapper");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let configuration = AgentLaunchSettings {
            executable: Some(executable.to_string_lossy().into_owned()),
            model: Some("fixture-model".into()),
            effort: Some("low".into()),
            permission_mode: Some("plan".into()),
            arguments: vec!["--fixture".into(), "two words".into()],
            environment: BTreeMap::from([("PROXY_URL".into(), "http://localhost:8080".into())]),
            ..Default::default()
        };
        let settings = super::super::AppSettings {
            agents: BTreeMap::from([("claude".into(), configuration.clone())]),
            default_shell: Some("/bin/sh".into()),
            ..Default::default()
        };
        let path = directory.path().join("settings.toml");
        super::super::save(&path, &settings).unwrap();
        assert_eq!(super::super::load(&path).unwrap(), settings);
        let mut invalid = configuration.clone();
        invalid.executable = Some(
            directory
                .path()
                .join("missing")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(invalid.validate(Provider::Claude).is_err());
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(configuration.validate(Provider::Claude).is_err());
        for name in [
            "INVALID-NAME",
            "9NAME",
            "CLAUDE_CONFIG_DIR",
            "CAGENTS_AUTO_ACCOUNT",
            "ANTHROPIC_API_KEY",
        ] {
            let invalid = AgentLaunchSettings {
                environment: BTreeMap::from([(name.into(), "value".into())]),
                ..Default::default()
            };
            assert!(invalid.validate(Provider::Claude).is_err(), "{name}");
        }
    }

    #[test]
    fn unavailable_saved_paths_preserve_policy_and_refuse_launch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = super::super::AppSettings::default();
        let mut value = toml::Value::try_from(&settings).unwrap();
        // Optional keys are absent from the serialized defaults; Value's
        // IndexMut only replaces an existing key rather than inserting one.
        value.as_table_mut().unwrap().insert(
            "default_shell".into(),
            toml::Value::String(
                directory
                    .path()
                    .join("missing-shell")
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        value.as_table_mut().unwrap().insert(
            "agents".into(),
            toml::Value::Table(toml::Table::from_iter([
                (
                    "claude".into(),
                    toml::Value::try_from(AgentLaunchSettings {
                        executable: Some(
                            directory
                                .path()
                                .join("missing-provider")
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        model: Some("pinned-model".into()),
                        permission_mode: Some("plan".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                ),
                (
                    "codex".into(),
                    toml::Value::try_from(AgentLaunchSettings {
                        model: Some("keep-model".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                ),
            ])),
        );
        std::fs::write(&path, toml::to_string(&value).unwrap()).unwrap();
        let loaded = super::super::load(&path).unwrap();
        assert_eq!(
            loaded.default_shell.as_deref(),
            value.get("default_shell").and_then(toml::Value::as_str)
        );
        assert!(loaded.agents["claude"].validate(Provider::Claude).is_err());
        assert_eq!(
            loaded.agents["claude"].model.as_deref(),
            Some("pinned-model")
        );
        assert_eq!(
            loaded.agents["claude"].permission_mode.as_deref(),
            Some("plan")
        );
        assert!(
            loaded.clone().validate().is_err(),
            "new saves must still refuse unusable launch settings"
        );
        assert_eq!(loaded.agents["codex"].model.as_deref(), Some("keep-model"));
    }

    #[test]
    fn multiple_unavailable_paths_can_be_repaired_and_saved_one_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = super::super::AppSettings {
            default_shell: Some("/missing/configured-shell".into()),
            agents: BTreeMap::from([
                (
                    "claude".into(),
                    AgentLaunchSettings {
                        executable: Some("/missing/claude-wrapper".into()),
                        permission_mode: Some("plan".into()),
                        ..Default::default()
                    },
                ),
                (
                    "codex".into(),
                    AgentLaunchSettings {
                        executable: Some("/missing/codex-wrapper".into()),
                        permission_mode: Some("readOnly".into()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        std::fs::write(&path, toml::to_string(&settings).unwrap()).unwrap();
        let mut current = super::super::load(&path).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let mut repaired = current.clone();
            repaired
                .agents
                .get_mut(provider_key(provider))
                .unwrap()
                .executable = Some("/bin/sh".into());
            repaired.clone().validate_transition(&current).unwrap();
            super::super::save_merged(&path, &current, &repaired).unwrap();
            current = super::super::load(&path).unwrap();
            assert_eq!(
                current.agents[provider_key(provider)].executable.as_deref(),
                Some("/bin/sh")
            );
        }
        let mut repaired = current.clone();
        repaired.default_shell = Some("/bin/sh".into());
        super::super::save_merged(&path, &current, &repaired).unwrap();
        let loaded = super::super::load(&path).unwrap();
        assert_eq!(loaded, repaired);
        assert_eq!(
            loaded.agents["claude"].permission_mode.as_deref(),
            Some("plan")
        );
        assert_eq!(
            loaded.agents["codex"].permission_mode.as_deref(),
            Some("readOnly")
        );
        loaded.validate().unwrap();
    }

    #[test]
    fn recorded_policy_syntax_is_independent_of_executable_availability() {
        let root = tempfile::tempdir().unwrap();
        let mut configuration = AgentLaunchSettings {
            executable: Some(root.path().join("missing-wrapper").to_str().unwrap().into()),
            environment: BTreeMap::from([("PATH".into(), "relative-tools".into())]),
            ..Default::default()
        };
        assert!(configuration.validate_syntax(Provider::Claude).is_ok());
        assert!(configuration.validate(Provider::Claude).is_err());
        configuration.executable = Some("relative-wrapper".into());
        assert!(configuration.validate_syntax(Provider::Claude).is_err());
        configuration.executable =
            Some(root.path().join("missing-wrapper").to_str().unwrap().into());
        configuration.permission_mode = Some("unknown-mode".into());
        assert!(configuration.validate_syntax(Provider::Claude).is_err());
    }

    #[test]
    fn configured_shell_is_used_by_ui_and_cli_recipe_factories() {
        const MARKER: &str = "CAGENTS_TEST_CONFIGURED_SHELL_DIRECTORY";
        let root = if let Some(root) = std::env::var_os(MARKER) {
            std::path::PathBuf::from(root)
        } else {
            let directory = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "settings::agents::tests::configured_shell_is_used_by_ui_and_cli_recipe_factories", "--nocapture", "--test-threads=1"])
                .env(MARKER, directory.path()).env("XDG_CONFIG_HOME", directory.path().join("config")).env("XDG_STATE_HOME", directory.path().join("state")).env("XDG_DATA_HOME", directory.path().join("data")).env("XDG_CACHE_HOME", directory.path().join("cache")).output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        };
        let shell = root.join("configured-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let settings = super::super::AppSettings {
            default_shell: Some(shell.to_string_lossy().into_owned()),
            agents: BTreeMap::from([(
                "claude".into(),
                AgentLaunchSettings {
                    executable: Some(shell.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        super::super::save(super::super::default_path().unwrap(), &settings).unwrap();
        let ui = LaunchRecipe::shell(root.clone()).unwrap();
        let cli = crate::control::terminal_spawn_recipe(root.clone(), vec![]).unwrap();
        assert_eq!(ui.program, shell.to_string_lossy());
        assert_eq!(cli.program, ui.program);
        #[cfg(any(feature = "ui", feature = "host"))]
        assert_eq!(
            provider_executable(
                Provider::Claude,
                Some(std::ffi::OsStr::new("/missing/provider/bin"))
            )
            .unwrap(),
            shell
        );
        let explicit = crate::control::terminal_spawn_recipe(
            root.clone(),
            vec!["/bin/echo".into(), "explicit".into()],
        )
        .unwrap();
        assert_eq!(explicit.program, "/bin/echo");
        assert_eq!(explicit.arguments, ["explicit"]);
        let path = super::super::default_path().unwrap();
        let valid_file = std::fs::read(&path).unwrap();
        std::fs::write(&path, "[invalid external edit").unwrap();
        assert_eq!(
            crate::model::terminal_shell(),
            Err(crate::model::LoginShellError::PreferencesUnavailable)
        );
        assert!(
            crate::control::terminal_spawn_recipe(root.clone(), vec![]).is_err(),
            "fresh CLI must not silently choose another shell when preferences cannot be read"
        );
        let captured = LaunchRecipe::shell_with_settings(root.clone(), &settings).unwrap();
        assert_eq!(
            captured.program,
            shell.to_string_lossy(),
            "running UI keeps its captured last valid shell policy"
        );
        assert!(
            crate::control::terminal_spawn_recipe(root.clone(), vec!["/bin/echo".into()]).is_ok()
        );
        #[cfg(feature = "host")]
        {
            let crate::device_service::Reply::Home {
                shell, shell_error, ..
            } = crate::device_service::execute(crate::device_service::Operation::Home).unwrap()
            else {
                panic!("device identity reply");
            };
            assert!(shell.is_empty());
            assert!(
                shell_error.is_some(),
                "remote identity remains available and records terminal policy refusal"
            );
        }
        std::fs::write(&path, valid_file).unwrap();
        std::fs::remove_file(&shell).unwrap();
        #[cfg(any(feature = "ui", feature = "host"))]
        assert!(
            provider_executable(
                Provider::Claude,
                Some(std::ffi::OsStr::new("/usr/bin:/bin"))
            )
            .is_err()
        );
        assert_eq!(
            crate::model::terminal_shell(),
            Err(crate::model::LoginShellError::ConfiguredUnavailable)
        );
        assert!(
            crate::model::login_shell().is_ok(),
            "a terminal-only stale pin cannot disable login-environment discovery"
        );
        #[cfg(feature = "host")]
        {
            let crate::device_service::Reply::Home {
                shell: reported, ..
            } = crate::device_service::execute(crate::device_service::Operation::Home).unwrap()
            else {
                panic!("device identity reply");
            };
            assert_eq!(
                reported,
                shell.to_string_lossy(),
                "remote identity must preserve unavailable pin without blocking unrelated operations"
            );
        }
        assert!(
            crate::control::terminal_spawn_recipe(root, vec![]).is_err(),
            "a missing pinned shell cannot silently switch to the login shell"
        );
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn explicit_permissions_refuse_unknown_and_cross_provider_modes() {
        let root = tempfile::tempdir().unwrap();
        for (provider, mode) in [
            (Provider::Claude, "readOnly"),
            (Provider::Claude, "future-mode"),
            (Provider::Codex, "plan"),
            (Provider::Codex, "future-mode"),
            (Provider::Omp, "workspaceWrite"),
            (Provider::Omp, "future-mode"),
        ] {
            let result = crate::chat::fresh_session_configured(
                provider,
                crate::model::TerminalId::new(),
                root.path().into(),
                &crate::account::Account::system(provider),
                &AgentLaunchSettings::default(),
                LaunchDefaults {
                    permission_mode: Some(mode),
                    ..Default::default()
                },
            );
            assert!(result.is_err(), "{provider:?} must refuse {mode}");
        }
        for (provider, modes) in [
            (Provider::Claude, &["default", "plan"][..]),
            (
                Provider::Codex,
                &[
                    "on-request",
                    "on-failure",
                    "untrusted",
                    "never",
                    "workspaceWrite",
                ][..],
            ),
            (Provider::Omp, &["always-ask", "write", "yolo"][..]),
        ] {
            for mode in modes {
                crate::chat::fresh_session_configured(
                    provider,
                    crate::model::TerminalId::new(),
                    root.path().into(),
                    &crate::account::Account::system(provider),
                    &AgentLaunchSettings::default(),
                    LaunchDefaults {
                        permission_mode: Some(mode),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn concurrent_first_provider_edits_merge_fields_without_losing_launch_policy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let base = super::super::AppSettings::default();
        let mut launch_window = base.clone();
        launch_window.agents.insert(
            "claude".into(),
            AgentLaunchSettings {
                executable: Some("/bin/sh".into()),
                arguments: vec!["--append-system-prompt".into(), "fixture".into()],
                environment: BTreeMap::from([("PROVIDER_FIXTURE_VALUE".into(), "kept".into())]),
                ..Default::default()
            },
        );
        super::super::save_merged(&path, &base, &launch_window).unwrap();
        let mut model_window = base.clone();
        model_window.agents.insert(
            "claude".into(),
            AgentLaunchSettings {
                model: Some("chosen-model".into()),
                ..Default::default()
            },
        );
        super::super::save_merged(&path, &base, &model_window).unwrap();
        let merged = super::super::load(&path).unwrap();
        let expected = AgentLaunchSettings {
            model: Some("chosen-model".into()),
            ..launch_window.agents["claude"].clone()
        };
        assert_eq!(merged.agents["claude"], expected);
        let mut reset = merged.clone();
        reset.agents.remove("claude");
        super::super::save_merged(&path, &merged, &reset).unwrap();
        assert!(
            !super::super::load(&path)
                .unwrap()
                .agents
                .contains_key("claude"),
            "explicit provider-table reset must still remove it"
        );
    }

    #[test]
    fn account_execution_validation_ignores_conversation_defaults_only() {
        let configuration = AgentLaunchSettings {
            model: Some("future\nmodel".into()),
            permission_mode: Some("future-mode".into()),
            ..Default::default()
        };
        assert!(configuration.validate(Provider::Claude).is_err());
        configuration.validate_execution(Provider::Claude).unwrap();
        let invalid = AgentLaunchSettings {
            executable: Some("/missing/pinned-wrapper".into()),
            ..configuration
        };
        assert!(invalid.validate_execution(Provider::Claude).is_err());
    }

    #[test]
    fn explicit_conversation_options_replace_defaults_without_duplicate_flags() {
        let configuration = AgentLaunchSettings {
            model: Some("default-model".into()),
            effort: Some("low".into()),
            permission_mode: Some("plan".into()),
            ..Default::default()
        };
        let mut arguments = vec![
            "--model".into(),
            "argument-model".into(),
            "--effort=high".into(),
        ];
        configuration.append_options(
            Provider::Claude,
            &mut arguments,
            LaunchDefaults {
                model: Some("conversation-model"),
                effort: Some("medium"),
                permission_mode: Some("manual"),
            },
        );
        assert_eq!(
            arguments,
            [
                "--model",
                "conversation-model",
                "--effort",
                "medium",
                "--permission-mode",
                "default"
            ]
        );
        let configuration = AgentLaunchSettings {
            effort: Some("low".into()),
            permission_mode: Some("workspaceWrite".into()),
            ..Default::default()
        };
        let mut arguments = vec![
            "--config".into(),
            "unrelated=true".into(),
            "--config=model_reasoning_effort=high".into(),
        ];
        configuration.append_options(Provider::Codex, &mut arguments, Default::default());
        assert_eq!(
            arguments,
            [
                "--config",
                "unrelated=true",
                "--config",
                "model_reasoning_effort=low",
                "--sandbox",
                "workspace-write",
                "--ask-for-approval",
                "on-request"
            ]
        );
    }

    #[test]
    fn composer_selection_replaces_or_clears_configured_model_and_effort() {
        let mut arguments = vec![
            "--model=default-model".into(),
            "--effort".into(),
            "low".into(),
            "--fixture".into(),
        ];
        set_model_options(
            &mut arguments,
            Provider::Claude,
            Some("chosen-model"),
            Some("high"),
        );
        assert_eq!(
            arguments,
            ["--fixture", "--model", "chosen-model", "--effort", "high"]
        );
        set_model_options(&mut arguments, Provider::Claude, None, None);
        assert_eq!(arguments, ["--fixture"]);
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn explicit_choices_override_retained_invalid_defaults_before_launch_validation() {
        let configuration = AgentLaunchSettings {
            model: Some("bad\nmodel".into()),
            permission_mode: Some("future-permission-mode".into()),
            ..Default::default()
        };
        let root = tempfile::tempdir().unwrap();
        let account = crate::account::Account::system(Provider::Claude);
        assert!(
            crate::chat::fresh_session_configured(
                Provider::Claude,
                crate::model::TerminalId::new(),
                root.path().into(),
                &account,
                &configuration,
                Default::default()
            )
            .is_err()
        );
        let session = crate::chat::fresh_session_configured(
            Provider::Claude,
            crate::model::TerminalId::new(),
            root.path().into(),
            &account,
            &configuration,
            LaunchDefaults {
                model: Some("chosen-model"),
                permission_mode: Some("plan"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            session
                .launch
                .recipe
                .arguments
                .windows(2)
                .any(|pair| pair == ["--model", "chosen-model"])
        );
        assert!(
            session
                .launch
                .recipe
                .arguments
                .windows(2)
                .any(|pair| pair == ["--permission-mode", "plan"])
        );
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn configured_launch_records_executable_arguments_environment_and_model() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = AgentLaunchSettings {
            executable: Some("/bin/sh".into()),
            model: Some("fixture-model".into()),
            effort: Some("low".into()),
            permission_mode: Some("plan".into()),
            arguments: vec!["--fixture-launch".into()],
            environment: BTreeMap::from([("PROXY_URL".into(), "http://localhost:8080".into())]),
            ..Default::default()
        };
        let session = crate::chat::fresh_session_configured(
            Provider::Claude,
            crate::model::TerminalId::new(),
            directory.path().into(),
            &crate::account::Account::system(Provider::Claude),
            &configuration,
            Default::default(),
        )
        .unwrap();
        assert_eq!(session.launch.recipe.program, "/bin/sh");
        assert!(
            session
                .launch
                .recipe
                .arguments
                .contains(&"--fixture-launch".to_owned())
        );
        assert!(
            session
                .launch
                .recipe
                .arguments
                .windows(2)
                .any(|pair| pair == ["--model", "fixture-model"])
        );
        assert_eq!(
            session
                .launch
                .recipe
                .environment
                .get("PROXY_URL")
                .map(String::as_str),
            Some("http://localhost:8080")
        );
    }
}
