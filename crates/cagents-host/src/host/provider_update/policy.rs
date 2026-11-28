use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use semver::Version;
use serde_json::Value;

use crate::{
    account::{home::Homes, registry::Store},
    agent::Provider,
};

use super::release::parse_version;

const CONFIG_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Policy {
    stable: bool,
    latest: bool,
    disabled: bool,
    automatic_disabled: bool,
    minimum: Option<Version>,
    maximum: Option<Version>,
}

impl Policy {
    pub(super) fn read() -> Result<Self, String> {
        let base =
            directories::BaseDirs::new().ok_or("Cannot locate the Claude Code configuration")?;
        let accounts = Store::installation()
            .ok_or("Cannot locate the Cagents account registry")?
            .load()
            .map_err(|error| error.to_string())?;
        let mut settings: Vec<PathBuf> = accounts
            .all_accounts(Homes::installation().as_ref())
            .into_iter()
            .filter(|account| account.provider == Provider::Claude)
            .flat_map(|account| {
                let explicit = account.launch_home();
                let config = explicit
                    .clone()
                    .unwrap_or_else(|| base.home_dir().join(".claude"));
                let global = explicit.unwrap_or_else(|| base.home_dir().to_owned());
                [
                    config.join("settings.json"),
                    config.join("remote-settings.json"),
                    global.join(".claude.json"),
                ]
            })
            .collect();
        settings.sort_unstable();
        settings.dedup();

        let managed = if cfg!(target_os = "macos") {
            Path::new("/Library/Application Support/ClaudeCode")
        } else {
            Path::new("/etc/claude-code")
        };
        settings.push(managed.join("managed-settings.json"));
        match fs::read_dir(managed.join("managed-settings.d")) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry.map_err(|error| error.to_string())?.path();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "json")
                        && !path
                            .file_name()
                            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
                    {
                        settings.push(path);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let mut policy = Self {
            disabled: std::env::var_os("DISABLE_UPDATES").is_some_and(|value| enabled(&value)),
            automatic_disabled: std::env::var_os("DISABLE_AUTOUPDATER")
                .is_some_and(|value| enabled(&value))
                || std::env::var_os("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
                    .is_some_and(|value| !value.is_empty()),
            ..Self::default()
        };
        for path in settings {
            if let Some(settings) = read_settings(&path)? {
                policy
                    .include(&settings)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
            }
        }
        Ok(policy)
    }

    fn include(&mut self, settings: &Value) -> Result<(), String> {
        let environment = &settings["env"];
        self.disabled |= configured_flag(environment, "DISABLE_UPDATES");
        self.automatic_disabled |= configured_flag(environment, "DISABLE_AUTOUPDATER")
            || environment["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            || settings["autoUpdates"] == false;
        match &settings["autoUpdatesChannel"] {
            Value::Null => {}
            value if value == "latest" => self.latest = true,
            value if value == "stable" => self.stable = true,
            _ => return Err("Unknown Claude Code update channel".into()),
        }
        for key in ["minimumVersion", "requiredMinimumVersion"] {
            if let Some(version) = version_setting(settings, key)? {
                self.minimum = self.minimum.take().max(Some(version));
            }
        }
        if let Some(version) = version_setting(settings, "requiredMaximumVersion")? {
            self.maximum = Some(
                self.maximum
                    .take()
                    .map_or(version.clone(), |old| old.min(version)),
            );
        }
        Ok(())
    }

    pub(super) fn channel(&self, automatic: bool) -> Result<&'static str, &'static str> {
        if self.disabled {
            return Err("Claude Code updates are disabled by DISABLE_UPDATES");
        }
        if automatic && self.automatic_disabled {
            return Err("Claude Code automatic updates are disabled");
        }
        match (self.stable, self.latest) {
            (true, true) => Err("Claude Code accounts specify different update channels"),
            (true, false) => Ok("stable"),
            (false, _) => Ok("latest"),
        }
    }

    pub(super) fn allows(&self, version: &Version) -> bool {
        self.minimum
            .as_ref()
            .is_none_or(|minimum| version >= minimum)
            && self
                .maximum
                .as_ref()
                .is_none_or(|maximum| version <= maximum)
    }
}

fn enabled(value: &std::ffi::OsStr) -> bool {
    value.to_str().is_some_and(|value| {
        ["1", "true", "yes", "on"]
            .into_iter()
            .any(|enabled| value.eq_ignore_ascii_case(enabled))
    })
}

fn configured_flag(environment: &Value, key: &str) -> bool {
    match &environment[key] {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64() == Some(1),
        Value::String(value) => enabled(value.as_ref()),
        _ => false,
    }
}

fn version_setting(settings: &Value, key: &str) -> Result<Option<Version>, String> {
    match &settings[key] {
        Value::Null => Ok(None),
        value => parse_version(value.as_str().ok_or_else(|| format!("Invalid {key}"))?)
            .map(Some)
            .map_err(|error| format!("Invalid {key}: {error}")),
    }
}

fn read_settings(path: &Path) -> Result<Option<Value>, String> {
    let describe = |error| {
        format!(
            "Cannot read Claude Code settings at {}: {error}",
            path.display()
        )
    };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(describe(error)),
    };
    if !file.metadata().map_err(describe)?.is_file() {
        return Err(format!(
            "Claude Code settings at {} are not a regular file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(describe)?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(format!(
            "Claude Code settings at {} exceed the size limit",
            path.display()
        ));
    }
    let settings: Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Invalid Claude Code settings at {}: {error}",
            path.display()
        )
    })?;
    if !settings.is_object() {
        return Err(format!(
            "Claude Code settings at {} are not a JSON object",
            path.display()
        ));
    }
    Ok(Some(settings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_cannot_bypass_a_pin_or_opt_out_in_another_config() {
        let mut policy = Policy::default();
        policy
            .include(&json!({"autoUpdatesChannel": "stable", "minimumVersion": "2.0.0"}))
            .unwrap();
        policy
            .include(
                &json!({"minimumVersion": "1.0.0", "requiredMaximumVersion": "2.1.0",
            "env": {"DISABLE_AUTOUPDATER": "1"}}),
            )
            .unwrap();
        assert!(policy.channel(true).is_err());
        assert_eq!(policy.channel(false), Ok("stable"));
        assert!(!policy.allows(&Version::new(1, 0, 0)));
        assert!(policy.allows(&Version::new(2, 0, 0)));
        assert!(!policy.allows(&Version::new(2, 2, 0)));
        policy
            .include(&json!({"env": {"DISABLE_UPDATES": "true"}}))
            .unwrap();
        assert!(policy.channel(false).is_err());
    }
}
