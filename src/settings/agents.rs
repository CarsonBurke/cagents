//! Provider launch preferences shared by the CLI, daemon and native composer.
use std::{collections::BTreeMap, ffi::CString, os::unix::ffi::OsStrExt, path::Path};
use serde::{Deserialize, Serialize};
use crate::{agent::Provider, model::LaunchRecipe};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLaunchSettings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub executable: Option<String>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

pub fn provider_key(provider: Provider) -> &'static str {
    match provider { Provider::Claude => "claude", Provider::Codex => "codex", Provider::Omp => "omp" }
}

/// Saved provider launch preferences, including unavailable pins that must
/// remain visible and must never silently fall back to another executable.
pub fn provider_settings(provider: Provider) -> Result<AgentLaunchSettings, String> {
    let path = super::default_path().map_err(|error| error.to_string())?;
    let settings = super::load(path).map_err(|error| error.to_string())?;
    Ok(settings.agents.get(provider_key(provider)).cloned().unwrap_or_default())
}

/// Account discovery and authentication use the same pinned CLI as launches.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn provider_executable(provider: Provider, search_path: Option<&std::ffi::OsStr>) -> Result<std::path::PathBuf, String> {
    provider_settings(provider)?.resolve_executable(provider, search_path)
}

pub fn permission_modes(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["manual", "acceptEdits", "auto", "plan", "dontAsk", "bypassPermissions"],
        Provider::Codex => &["readOnly", "workspaceWrite", "yolo"],
        Provider::Omp => &[],
    }
}

/// Explicit CLI requests also accept provider approval policies that are not
/// conversation presets. Keep this distinct from the saved composer choices.
pub fn validate_permission_override(provider: Provider, mode: &str) -> Result<(), String> {
    let supported = permission_modes(provider).contains(&mode) || match provider {
        Provider::Claude => mode == "default",
        // Preserve approval-policy values understood by recorded Codex recipes,
        // including policies from older CLI versions.
        Provider::Codex => matches!(mode, "on-request" | "on-failure" | "untrusted" | "never"),
        Provider::Omp => matches!(mode, "always-ask" | "write" | "yolo"),
    };
    if supported { Ok(()) } else { Err(format!("Choose a permission mode offered by {}.", provider.label())) }
}

/// Executable paths are absolute so a launch in another worktree means the
/// same thing. `access` checks the current user's actual execute permission.
pub fn executable_available(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.is_file() && CString::new(path.as_os_str().as_bytes()).is_ok_and(|path| {
        // SAFETY: this is a live, NUL-terminated path and access retains no pointer.
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    })
}

pub fn environment_name_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub fn environment_reserved(provider: Provider, name: &str) -> bool {
    name.starts_with("CAGENTS_")
        || matches!(name, "CLAUDE_CONFIG_DIR" | "CODEX_HOME")
        || crate::account::credential_overrides(provider).contains(&name)
}

impl AgentLaunchSettings {
    /// Resolve from this immutable policy capture so probes cannot pair an
    /// executable from one settings revision with arguments from another.
    #[cfg(any(feature = "ui", feature = "host"))]
    pub fn resolve_executable(&self, provider: Provider, search_path: Option<&std::ffi::OsStr>) -> Result<std::path::PathBuf, String> {
        if let Some(executable) = &self.executable {
            if !executable_available(executable) {
                return Err(format!("{} executable must be an absolute path to an executable file.", provider.label()));
            }
            return Ok(executable.into());
        }
        crate::account::probe::find_executable(provider.executable(), search_path).ok_or_else(|| format!("{} is not installed", provider.label()))
    }

    /// Authentication and discovery consume execution policy without model,
    /// effort or permission defaults, which are conversation choices.
    pub fn validate_execution(&self, provider: Provider) -> Result<(), String> {
        if self.executable.as_deref().is_some_and(|path| !executable_available(path)) {
            return Err(format!("{} executable must be an absolute path to an executable file.", provider.label()));
        }
        for (name, value) in &self.environment {
            if !environment_name_valid(name) { return Err("Environment names must start with a letter or underscore and contain only letters, numbers and underscores.".into()); }
            if environment_reserved(provider, name) { return Err(format!("{name} is reserved for account isolation and cannot be overridden.")); }
            if value.contains('\0') { return Err("Environment values cannot contain a NUL character.".into()); }
        }
        if self.arguments.iter().any(|argument| argument.contains('\0')) { return Err("Arguments cannot contain a NUL character.".into()); }
        Ok(())
    }

    pub fn validate(&self, provider: Provider) -> Result<(), String> {
        self.validate_execution(provider)?;
        for value in self.model.iter().chain(self.effort.iter()) {
            if value.trim().is_empty() || value.chars().any(char::is_control) { return Err("Model and effort must be non-empty names without control characters.".into()); }
        }
        if self.permission_mode.as_deref().is_some_and(|mode| !permission_modes(provider).contains(&mode)) {
            return Err(format!("Choose a permission mode offered by {}.", provider.label()));
        }
        Ok(())
    }

    /// Validate changed fields independently so retained invalid siblings can
    /// be repaired one control at a time without weakening launch validation.
    pub fn validate_changes(&self, previous: &Self, provider: Provider) -> Result<(), String> {
        Self {
            model: (self.model != previous.model).then(|| self.model.clone()).flatten(),
            effort: (self.effort != previous.effort).then(|| self.effort.clone()).flatten(),
            permission_mode: (self.permission_mode != previous.permission_mode).then(|| self.permission_mode.clone()).flatten(),
            executable: (self.executable != previous.executable).then(|| self.executable.clone()).flatten(),
            arguments: if self.arguments != previous.arguments { self.arguments.clone() } else { vec![] },
            environment: if self.environment != previous.environment { self.environment.clone() } else { Default::default() },
        }.validate(provider)
    }

    /// Apply before account isolation. The selected home and credential
    /// removals always have the final say over inherited launch environment.
    pub fn apply_launch(&self, recipe: &mut LaunchRecipe) {
        if let Some(executable) = &self.executable { recipe.program.clone_from(executable); }
        recipe.arguments.extend(self.arguments.iter().cloned());
        recipe.environment.extend(self.environment.clone());
    }

    pub fn append_options(&self, provider: Provider, arguments: &mut Vec<String>, overrides: LaunchDefaults<'_>) {
        if let Some(model) = overrides.model.or(self.model.as_deref()) { set_option(arguments, "--model", model); }
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
        if let Some(mode) = overrides.permission_mode.or(self.permission_mode.as_deref()) {
            match provider {
                Provider::Claude => {
                    arguments.retain(|argument| argument != "--dangerously-skip-permissions");
                    set_option(arguments, "--permission-mode", if mode == "manual" { "default" } else { mode });
                }
                Provider::Codex => {
                    arguments.retain(|argument| !matches!(argument.as_str(), "--yolo" | "--dangerously-bypass-approvals-and-sandbox"));
                    if mode == "yolo" { arguments.push("--dangerously-bypass-approvals-and-sandbox".into()); }
                    else if matches!(mode, "readOnly" | "workspaceWrite") {
                        set_option(arguments, "--sandbox", if mode == "readOnly" { "read-only" } else { "workspace-write" });
                        set_option(arguments, "--ask-for-approval", "on-request");
                    } else { set_option(arguments, "--ask-for-approval", mode); }
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
        if arguments[index] == flag { arguments.remove(index); if index < arguments.len() { arguments.remove(index); } }
        else if arguments[index].strip_prefix(flag).is_some_and(|suffix| suffix.starts_with('=')) { arguments.remove(index); }
        else { index += 1; }
    }
}

fn set_option(arguments: &mut Vec<String>, flag: &str, value: &str) {
    remove_option(arguments, flag);
    arguments.extend([flag.into(), value.into()]);
}

/// Record the composer/provider-confirmed selection in the launch recipe.
/// None deliberately clears a configured default when the user chooses Automatic.
pub fn set_model_options(arguments: &mut Vec<String>, provider: Provider, model: Option<&str>, effort: Option<&str>) {
    remove_option(arguments, "--model");
    match provider {
        Provider::Claude => remove_option(arguments, "--effort"),
        Provider::Omp => remove_option(arguments, "--thinking"),
        Provider::Codex => remove_config_key(arguments, crate::chat::CODEX_REASONING_EFFORT_KEY),
    }
    AgentLaunchSettings::default().append_options(provider, arguments, LaunchDefaults { model, effort, permission_mode: None });
}

fn remove_config_key(arguments: &mut Vec<String>, key: &str) {
    let matches_key = |value: &str| value.split_once('=').is_some_and(|(name, _)| name.trim() == key);
    let mut index = 0;
    while index < arguments.len() {
        if matches!(arguments[index].as_str(), "-c" | "--config") && arguments.get(index + 1).is_some_and(|value| matches_key(value)) {
            arguments.drain(index..index + 2);
        } else if arguments[index].strip_prefix("--config=").is_some_and(matches_key) { arguments.remove(index); }
        else { index += 1; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn provider_tables_round_trip_and_reject_unsafe_launch_values() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("provider-wrapper");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let configuration = AgentLaunchSettings {
            executable: Some(executable.to_string_lossy().into_owned()),
            model: Some("fixture-model".into()), effort: Some("low".into()), permission_mode: Some("plan".into()),
            arguments: vec!["--fixture".into(), "two words".into()],
            environment: BTreeMap::from([("PROXY_URL".into(), "http://localhost:8080".into())]),
        };
        let settings = super::super::AppSettings { agents: BTreeMap::from([("claude".into(), configuration.clone())]), default_shell: Some("/bin/sh".into()), ..Default::default() };
        let path = directory.path().join("settings.toml");
        super::super::save(&path, &settings).unwrap();
        assert_eq!(super::super::load(&path).unwrap(), settings);
        let mut invalid = configuration.clone();
        invalid.executable = Some(directory.path().join("missing").to_string_lossy().into_owned());
        assert!(invalid.validate(Provider::Claude).is_err());
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(configuration.validate(Provider::Claude).is_err());
        for name in ["INVALID-NAME", "9NAME", "CLAUDE_CONFIG_DIR", "CAGENTS_AUTO_ACCOUNT", "ANTHROPIC_API_KEY"] {
            let invalid = AgentLaunchSettings { environment: BTreeMap::from([(name.into(), "value".into())]), ..Default::default() };
            assert!(invalid.validate(Provider::Claude).is_err(), "{name}");
        }
    }

    #[test]
    fn unavailable_saved_paths_preserve_policy_and_refuse_launch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = super::super::AppSettings::default();
        let mut value = toml::Value::try_from(&settings).unwrap();
        value["default_shell"] = toml::Value::String(directory.path().join("missing-shell").to_string_lossy().into_owned());
        value["agents"] = toml::Value::Table(toml::Table::from_iter([
            ("claude".into(), toml::Value::try_from(AgentLaunchSettings { executable: Some(directory.path().join("missing-provider").to_string_lossy().into_owned()), model: Some("pinned-model".into()), permission_mode: Some("plan".into()), ..Default::default() }).unwrap()),
            ("codex".into(), toml::Value::try_from(AgentLaunchSettings { model: Some("keep-model".into()), ..Default::default() }).unwrap()),
        ]));
        std::fs::write(&path, toml::to_string(&value).unwrap()).unwrap();
        let loaded = super::super::load(&path).unwrap();
        assert_eq!(loaded.default_shell.as_deref(), value.get("default_shell").and_then(toml::Value::as_str));
        assert!(loaded.agents["claude"].validate(Provider::Claude).is_err());
        assert_eq!(loaded.agents["claude"].model.as_deref(), Some("pinned-model"));
        assert_eq!(loaded.agents["claude"].permission_mode.as_deref(), Some("plan"));
        assert!(loaded.clone().validate().is_err(), "new saves must still refuse unusable launch settings");
        assert_eq!(loaded.agents["codex"].model.as_deref(), Some("keep-model"));
    }

    #[test]
    fn multiple_unavailable_paths_can_be_repaired_and_saved_one_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = super::super::AppSettings {
            default_shell: Some("/missing/configured-shell".into()),
            agents: BTreeMap::from([
                ("claude".into(), AgentLaunchSettings { executable: Some("/missing/claude-wrapper".into()), permission_mode: Some("plan".into()), ..Default::default() }),
                ("codex".into(), AgentLaunchSettings { executable: Some("/missing/codex-wrapper".into()), permission_mode: Some("readOnly".into()), ..Default::default() }),
            ]), ..Default::default()
        };
        std::fs::write(&path, toml::to_string(&settings).unwrap()).unwrap();
        let mut current = super::super::load(&path).unwrap();
        for provider in [Provider::Claude, Provider::Codex] {
            let mut repaired = current.clone();
            repaired.agents.get_mut(provider_key(provider)).unwrap().executable = Some("/bin/sh".into());
            repaired.clone().validate_transition(&current).unwrap();
            super::super::save_merged(&path, &current, &repaired).unwrap();
            current = super::super::load(&path).unwrap();
            assert_eq!(current.agents[provider_key(provider)].executable.as_deref(), Some("/bin/sh"));
        }
        let mut repaired = current.clone(); repaired.default_shell = Some("/bin/sh".into());
        super::super::save_merged(&path, &current, &repaired).unwrap();
        let loaded = super::super::load(&path).unwrap();
        assert_eq!(loaded, repaired);
        assert_eq!(loaded.agents["claude"].permission_mode.as_deref(), Some("plan"));
        assert_eq!(loaded.agents["codex"].permission_mode.as_deref(), Some("readOnly"));
        loaded.validate().unwrap();
    }

    #[test]
    fn configured_shell_is_used_by_ui_and_cli_recipe_factories() {
        const MARKER: &str = "CAGENTS_TEST_CONFIGURED_SHELL_DIRECTORY";
        let root = if let Some(root) = std::env::var_os(MARKER) { std::path::PathBuf::from(root) } else {
            let directory = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "settings::agents::tests::configured_shell_is_used_by_ui_and_cli_recipe_factories", "--nocapture", "--test-threads=1"])
                .env(MARKER, directory.path()).env("XDG_CONFIG_HOME", directory.path().join("config")).env("XDG_STATE_HOME", directory.path().join("state")).env("XDG_DATA_HOME", directory.path().join("data")).env("XDG_CACHE_HOME", directory.path().join("cache")).output().unwrap();
            assert!(result.status.success(), "{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
            return;
        };
        let shell = root.join("configured-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let settings = super::super::AppSettings { default_shell: Some(shell.to_string_lossy().into_owned()), agents: BTreeMap::from([("claude".into(), AgentLaunchSettings { executable: Some(shell.to_string_lossy().into_owned()), ..Default::default() })]), ..Default::default() };
        super::super::save(super::super::default_path().unwrap(), &settings).unwrap();
        let ui = LaunchRecipe::shell(root.clone()).unwrap();
        let cli = crate::control::terminal_spawn_recipe(root.clone(), vec![]).unwrap();
        assert_eq!(ui.program, shell.to_string_lossy());
        assert_eq!(cli.program, ui.program);
        #[cfg(any(feature = "ui", feature = "host"))]
        assert_eq!(provider_executable(Provider::Claude, Some(std::ffi::OsStr::new("/missing/provider/bin"))).unwrap(), shell);
        let explicit = crate::control::terminal_spawn_recipe(root.clone(), vec!["/bin/echo".into(), "explicit".into()]).unwrap();
        assert_eq!(explicit.program, "/bin/echo");
        assert_eq!(explicit.arguments, ["explicit"]);
        std::fs::remove_file(&shell).unwrap();
        #[cfg(any(feature = "ui", feature = "host"))]
        assert!(provider_executable(Provider::Claude, Some(std::ffi::OsStr::new("/usr/bin:/bin"))).is_err());
        assert_eq!(crate::model::login_shell(), Err(crate::model::LoginShellError::ConfiguredUnavailable));
        assert!(crate::model::system_login_shell().is_ok(), "a terminal-only stale pin cannot disable login-environment discovery");
        assert!(crate::control::terminal_spawn_recipe(root, vec![]).is_err(), "a missing pinned shell cannot silently switch to the login shell");
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn explicit_permissions_refuse_unknown_and_cross_provider_modes() {
        let root = tempfile::tempdir().unwrap();
        for (provider, mode) in [
            (Provider::Claude, "readOnly"), (Provider::Claude, "future-mode"),
            (Provider::Codex, "plan"), (Provider::Codex, "future-mode"),
            (Provider::Omp, "workspaceWrite"), (Provider::Omp, "future-mode"),
        ] {
            let result = crate::chat::fresh_session_configured(provider, crate::model::TerminalId::new(), root.path().into(), &crate::account::Account::system(provider), &AgentLaunchSettings::default(), LaunchDefaults { permission_mode: Some(mode), ..Default::default() });
            assert!(result.is_err(), "{provider:?} must refuse {mode}");
        }
        for (provider, modes) in [
            (Provider::Claude, &["default", "plan"][..]),
            (Provider::Codex, &["on-request", "on-failure", "untrusted", "never", "workspaceWrite"][..]),
            (Provider::Omp, &["always-ask", "write", "yolo"][..]),
        ] {
            for mode in modes {
                crate::chat::fresh_session_configured(provider, crate::model::TerminalId::new(), root.path().into(), &crate::account::Account::system(provider), &AgentLaunchSettings::default(), LaunchDefaults { permission_mode: Some(mode), ..Default::default() }).unwrap();
            }
        }
    }

    #[test]
    fn concurrent_first_provider_edits_merge_fields_without_losing_launch_policy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let base = super::super::AppSettings::default();
        let mut launch_window = base.clone();
        launch_window.agents.insert("claude".into(), AgentLaunchSettings {
            executable: Some("/bin/sh".into()), arguments: vec!["--append-system-prompt".into(), "fixture".into()],
            environment: BTreeMap::from([("PROVIDER_FIXTURE_VALUE".into(), "kept".into())]), ..Default::default()
        });
        super::super::save_merged(&path, &base, &launch_window).unwrap();
        let mut model_window = base.clone();
        model_window.agents.insert("claude".into(), AgentLaunchSettings { model: Some("chosen-model".into()), ..Default::default() });
        super::super::save_merged(&path, &base, &model_window).unwrap();
        let merged = super::super::load(&path).unwrap();
        let expected = AgentLaunchSettings { model: Some("chosen-model".into()), ..launch_window.agents["claude"].clone() };
        assert_eq!(merged.agents["claude"], expected);
        let mut reset = merged.clone(); reset.agents.remove("claude");
        super::super::save_merged(&path, &merged, &reset).unwrap();
        assert!(!super::super::load(&path).unwrap().agents.contains_key("claude"), "explicit provider-table reset must still remove it");
    }

    #[test]
    fn account_execution_validation_ignores_conversation_defaults_only() {
        let configuration = AgentLaunchSettings { model: Some("future\nmodel".into()), permission_mode: Some("future-mode".into()), ..Default::default() };
        assert!(configuration.validate(Provider::Claude).is_err());
        configuration.validate_execution(Provider::Claude).unwrap();
        let invalid = AgentLaunchSettings { executable: Some("/missing/pinned-wrapper".into()), ..configuration };
        assert!(invalid.validate_execution(Provider::Claude).is_err());
    }

    #[test]
    fn explicit_conversation_options_replace_defaults_without_duplicate_flags() {
        let configuration = AgentLaunchSettings { model: Some("default-model".into()), effort: Some("low".into()), permission_mode: Some("plan".into()), ..Default::default() };
        let mut arguments = vec!["--model".into(), "argument-model".into(), "--effort=high".into()];
        configuration.append_options(Provider::Claude, &mut arguments, LaunchDefaults { model: Some("conversation-model"), effort: Some("medium"), permission_mode: Some("manual") });
        assert_eq!(arguments, ["--model", "conversation-model", "--effort", "medium", "--permission-mode", "default"]);
        let configuration = AgentLaunchSettings { effort: Some("low".into()), permission_mode: Some("workspaceWrite".into()), ..Default::default() };
        let mut arguments = vec!["--config".into(), "unrelated=true".into(), "--config=model_reasoning_effort=high".into()];
        configuration.append_options(Provider::Codex, &mut arguments, Default::default());
        assert_eq!(arguments, ["--config", "unrelated=true", "--config", "model_reasoning_effort=low", "--sandbox", "workspace-write", "--ask-for-approval", "on-request"]);
    }

    #[test]
    fn composer_selection_replaces_or_clears_configured_model_and_effort() {
        let mut arguments = vec!["--model=default-model".into(), "--effort".into(), "low".into(), "--fixture".into()];
        set_model_options(&mut arguments, Provider::Claude, Some("chosen-model"), Some("high"));
        assert_eq!(arguments, ["--fixture", "--model", "chosen-model", "--effort", "high"]);
        set_model_options(&mut arguments, Provider::Claude, None, None);
        assert_eq!(arguments, ["--fixture"]);
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn explicit_choices_override_retained_invalid_defaults_before_launch_validation() {
        let configuration = AgentLaunchSettings { model: Some("bad\nmodel".into()), permission_mode: Some("future-permission-mode".into()), ..Default::default() };
        let root = tempfile::tempdir().unwrap();
        let account = crate::account::Account::system(Provider::Claude);
        assert!(crate::chat::fresh_session_configured(Provider::Claude, crate::model::TerminalId::new(), root.path().into(), &account, &configuration, Default::default()).is_err());
        let session = crate::chat::fresh_session_configured(Provider::Claude, crate::model::TerminalId::new(), root.path().into(), &account, &configuration, LaunchDefaults { model: Some("chosen-model"), permission_mode: Some("plan"), ..Default::default() }).unwrap();
        assert!(session.launch.recipe.arguments.windows(2).any(|pair| pair == ["--model", "chosen-model"]));
        assert!(session.launch.recipe.arguments.windows(2).any(|pair| pair == ["--permission-mode", "plan"]));
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn configured_launch_records_executable_arguments_environment_and_model() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = AgentLaunchSettings { executable: Some("/bin/sh".into()), model: Some("fixture-model".into()), effort: Some("low".into()), permission_mode: Some("plan".into()), arguments: vec!["--fixture-launch".into()], environment: BTreeMap::from([("PROXY_URL".into(), "http://localhost:8080".into())]) };
        let session = crate::chat::fresh_session_configured(Provider::Claude, crate::model::TerminalId::new(), directory.path().into(), &crate::account::Account::system(Provider::Claude), &configuration, Default::default()).unwrap();
            assert_eq!(session.launch.recipe.program, "/bin/sh");
            assert!(session.launch.recipe.arguments.contains(&"--fixture-launch".to_owned()));
            assert!(session.launch.recipe.arguments.windows(2).any(|pair| pair == ["--model", "fixture-model"]));
        assert_eq!(session.launch.recipe.environment.get("PROXY_URL").map(String::as_str), Some("http://localhost:8080"));
    }
}
