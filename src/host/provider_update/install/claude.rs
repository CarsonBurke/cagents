use std::os::unix::fs::{OpenOptionsExt, symlink};

use semver::Version;
use serde_json::Value;

use super::*;

const RELEASES: &str = "https://downloads.claude.ai/claude-code-releases";
const MANAGED_CONFIG: &str = "/etc/claude-code";
const CONFIG_LIMIT: u64 = 8 * 1024 * 1024;

pub(super) async fn prepare(
    status: &ProviderStatus,
    context: &CommandContext<'_>,
    automatic: bool,
) -> Result<Preparation, String> {
    let base = directories::BaseDirs::new().ok_or("Cannot find the Claude installation home")?;
    let home = base.home_dir();
    let policy = load_live_policy(home)?;
    if let Some(reason) = policy.opt_out(&|name| std::env::var_os(name), automatic) {
        return Ok(Preparation::Skipped(reason));
    }
    let preparation = prepare_for_home(status, context, home, &policy, automatic).await?;
    if matches!(&preparation, Preparation::Ready(_))
        && load_live_policy(home)?.channel() != policy.channel()
    {
        return Ok(Preparation::Skipped(
            "Claude's release channel changed during the update check".into(),
        ));
    }
    Ok(preparation)
}

pub(super) fn opt_out(automatic: bool, expected_version: &str) -> Result<Option<String>, String> {
    let base = directories::BaseDirs::new().ok_or("Cannot find the Claude installation home")?;
    let policy = load_live_policy(base.home_dir())?;
    let version = Version::parse(expected_version).map_err(|error| error.to_string())?;
    Ok(policy
        .opt_out(&|name| std::env::var_os(name), automatic)
        .or_else(|| policy.exclusion(&version)))
}

fn load_live_policy(home: &Path) -> Result<Policy, String> {
    Policy::load(home, config_override(), Path::new(MANAGED_CONFIG))
}

fn config_override() -> Option<PathBuf> {
    crate::account::Account::system(Provider::Claude).launch_home()
}

struct Policy {
    user: Value,
    global: Value,
    managed: Vec<Value>,
}

impl Policy {
    fn load(
        home: &Path,
        config_override: Option<PathBuf>,
        managed_directory: &Path,
    ) -> Result<Self, String> {
        let global = config_override
            .as_deref()
            .unwrap_or(home)
            .join(".claude.json");
        let config = config_override.unwrap_or_else(|| home.join(".claude"));
        let user = load_config(&config.join("settings.json"))?;
        let global = load_config(&global)?;
        let mut managed = vec![load_config(&config.join("remote-settings.json"))?];
        // Claude loads drop-ins alphabetically. Search the latest override
        // first, then the main system file, for each update setting.
        let drop_ins = managed_directory.join("managed-settings.d");
        match fs::read_dir(&drop_ins) {
            Ok(entries) => {
                let mut files = entries
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<io::Result<Vec<_>>>()
                    .map_err(fs_error)?;
                files.sort_unstable();
                for file in files.into_iter().rev().filter(|path| {
                    path.extension() == Some(OsStr::new("json"))
                        && path
                            .file_name()
                            .is_some_and(|name| !name.as_encoded_bytes().starts_with(b"."))
                }) {
                    managed.push(load_config(&file)?);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(fs_error(error)),
        }
        managed.push(load_config(
            &managed_directory.join("managed-settings.json"),
        )?);
        Ok(Self {
            user,
            global,
            managed,
        })
    }

    fn settings(&self) -> impl Iterator<Item = &Value> {
        self.managed.iter().chain([&self.user, &self.global])
    }

    fn opt_out(
        &self,
        environment: &impl Fn(&str) -> Option<std::ffi::OsString>,
        automatic: bool,
    ) -> Option<String> {
        let configured = |name: &str| {
            self.settings()
                .find_map(|settings| settings["env"][name].as_str())
        };
        let enabled = |name: &str| {
            environment(name)
                .as_deref()
                .and_then(OsStr::to_str)
                .is_some_and(truthy)
                || configured(name).is_some_and(truthy)
        };
        if enabled("DISABLE_UPDATES") {
            return Some("Claude updates are disabled by DISABLE_UPDATES".into());
        }
        let no_background_traffic = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
        if automatic
            && (enabled("DISABLE_AUTOUPDATER")
                || environment(no_background_traffic).is_some_and(|value| !value.is_empty())
                || configured(no_background_traffic).is_some_and(|value| !value.is_empty())
                || self
                    .settings()
                    .any(|config| config["autoUpdates"].as_bool() == Some(false)))
        {
            return Some("Claude automatic updates are turned off".into());
        }
        None
    }

    fn setting(&self, name: &str) -> Option<&str> {
        self.settings().find_map(|settings| settings[name].as_str())
    }

    fn channel(&self) -> &str {
        match self.setting("autoUpdatesChannel") {
            Some("stable") => "stable",
            _ => "latest",
        }
    }

    fn exclusion(&self, version: &Version) -> Option<String> {
        let outside_bounds = self
            .setting("minimumVersion")
            .and_then(|value| Version::parse(value).ok())
            .is_some_and(|minimum| version < &minimum)
            || self
                .managed
                .iter()
                .find_map(|settings| settings["requiredMaximumVersion"].as_str())
                .and_then(|value| Version::parse(value).ok())
                .is_some_and(|maximum| version > &maximum);
        outside_bounds.then(|| format!("Claude's version settings exclude release {version}"))
    }
}

fn truthy(value: &str) -> bool {
    ["1", "true", "yes", "on"]
        .iter()
        .any(|enabled| value.eq_ignore_ascii_case(enabled))
}

fn load_config(path: &Path) -> Result<Value, String> {
    let error = |error: &dyn std::fmt::Display| {
        format!(
            "Cannot read Claude update settings at {}: {error}",
            path.display()
        )
    };
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(problem) => return Err(error(&problem)),
    };
    let metadata = file.metadata().map_err(|problem| error(&problem))?;
    if !metadata.is_file() || metadata.len() > CONFIG_LIMIT {
        return Err(error(&"expected a regular JSON file of at most 8 MiB"));
    }
    serde_json::from_reader(io::BufReader::new(file.take(CONFIG_LIMIT + 1)))
        .map_err(|problem| error(&problem))
}

async fn prepare_for_home(
    status: &ProviderStatus,
    context: &CommandContext<'_>,
    home: &Path,
    policy: &Policy,
    automatic: bool,
) -> Result<Preparation, String> {
    let launcher_dir = home.join(".local/bin").canonicalize().map_err(fs_error)?;
    let launcher = launcher_dir.join("claude");
    fs::read_link(&launcher).map_err(fs_error)?;
    let launcher_stamp = EntryStamp::read(&launcher).map_err(fs_error)?;
    let installation = SelectedInstall::capture(status, context.search_path)?;
    if launcher.canonicalize().map_err(fs_error)? != installation.resolved {
        return Err("Claude's native launcher changed during its update check".into());
    }
    let versions = parent(&installation.resolved)?;
    let installed = status
        .version
        .as_deref()
        .and_then(|version| Version::parse(version).ok())
        .ok_or("Claude's installed version is unknown")?;
    let channel = policy.channel();
    let curl = crate::account::probe::find_executable("curl", context.search_path)
        .ok_or("curl is required to check Claude releases")?;
    let mut check = context.command(&curl);
    check
        .args(["-q", "--fail", "--silent", "--show-error", "--location"])
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "30",
            "--max-filesize",
            "128",
        ])
        .arg(format!("{RELEASES}/{channel}"));
    let output = run_update(check).await?;
    let next = std::str::from_utf8(&output)
        .ok()
        .and_then(|output| Version::parse(output.trim()).ok())
        .ok_or("Claude's release channel did not report a version")?;
    let expected = next.to_string();
    if installed == next {
        installation.check(context.search_path)?;
        return Ok(Preparation::Current(expected));
    }
    if let Some(reason) = policy.exclusion(&next) {
        return Ok(Preparation::Skipped(reason));
    }
    // The minimum version is a floor: meeting it never requires replacing a
    // newer installation with an older release.
    if (automatic || channel == "latest") && next.cmp_precedence(&installed).is_lt() {
        return Ok(Preparation::Skipped(format!(
            "Claude's updater selected older release {expected}"
        )));
    }

    // Let Claude install and verify the pinned release in a private home.
    // Its download and all installer side effects precede the launch fence.
    let stage = stage_in(versions)?;
    let staged_home = stage.path();
    let data = staged_home.join(".local/share");
    let mut command = isolated_installer(
        &installation.resolved,
        &[
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
            "NODE_EXTRA_CA_CERTS",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ],
        std::env::vars_os(),
    );
    command
        .env(
            "PATH",
            context
                .search_path
                .unwrap_or_else(|| OsStr::new("/usr/bin:/bin")),
        )
        .env("HOME", staged_home)
        .env("CLAUDE_CONFIG_DIR", staged_home.join(".claude"))
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CACHE_HOME", staged_home.join(".cache"))
        .env("XDG_CONFIG_HOME", staged_home.join(".config"))
        .env("XDG_STATE_HOME", staged_home.join(".local/state"))
        .env("DISABLE_TELEMETRY", "1")
        .current_dir(staged_home)
        .args(["install", &expected]);
    stage.protect(&mut command);
    run_update(command).await?;

    let staged_binary = data.join("claude/versions").join(&expected);
    let staged_launcher = staged_home.join(".local/bin/claude");
    if staged_launcher.canonicalize().map_err(fs_error)? != staged_binary {
        return Err("Claude's staged launcher did not select the requested release".into());
    }
    let actual = stage
        .version(status.provider, &staged_binary, context.search_path)
        .await?;
    if actual != expected {
        return Err(format!(
            "Claude's staged release reports {actual}; expected {expected}"
        ));
    }

    let live_binary = versions.join(&expected);
    let launcher_stage = stage_in(&launcher_dir)?;
    let new_launcher = launcher_stage.path().join("claude");
    symlink(&live_binary, &new_launcher).map_err(fs_error)?;
    installation.check(context.search_path)?;
    if EntryStamp::read(&launcher).map_err(fs_error)? != launcher_stamp {
        return Err("Claude's native launcher changed during its update check".into());
    }
    let replacements = vec![
        Replacement::new(staged_binary, live_binary)?,
        Replacement::symlink(new_launcher, launcher)?,
    ];
    let update = PreparedUpdate::staged(installation, stage, replacements, expected);
    Ok(Preparation::Ready(update.hold_stage(launcher_stage)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::provider_update::install::tests::{
        fixture_path, fixture_status, write_executable,
    };

    #[test]
    fn provider_settings_and_environment_respect_automatic_update_consent() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("claude-config");
        let managed = home.path().join("managed");
        fs::create_dir(&config).unwrap();
        fs::write(config.join(".claude.json"), r#"{"autoUpdates":false}"#).unwrap();
        let policy = Policy::load(home.path(), Some(config.clone()), &managed).unwrap();
        assert!(policy.opt_out(&|_| None, true).is_some());
        assert!(policy.opt_out(&|_| None, false).is_none());
        fs::remove_file(config.join(".claude.json")).unwrap();

        fs::write(
            config.join("settings.json"),
            r#"{"env":{"DISABLE_AUTOUPDATER":"1"},"autoUpdatesChannel":"stable","minimumVersion":"2.0.0"}"#,
        )
        .unwrap();
        let policy = Policy::load(home.path(), Some(config.clone()), &managed).unwrap();
        assert!(policy.opt_out(&|_| None, true).is_some());
        assert_eq!(policy.channel(), "stable");
        assert!(policy.exclusion(&Version::new(1, 0, 0)).is_some());
        assert!(policy.exclusion(&Version::new(2, 0, 0)).is_none());
        fs::remove_file(config.join("settings.json")).unwrap();

        fs::create_dir(&managed).unwrap();
        fs::write(
            managed.join("managed-settings.json"),
            r#"{"requiredMaximumVersion":"1.0.0"}"#,
        )
        .unwrap();
        let policy = Policy::load(home.path(), Some(config), &managed).unwrap();
        assert!(policy.exclusion(&Version::new(2, 0, 0)).is_some());
        let opt_out = |name: &str| (name == "DISABLE_AUTOUPDATER").then(|| "yes".into());
        assert!(policy.opt_out(&opt_out, true).is_some());
        assert!(policy.opt_out(&opt_out, false).is_none());
        let opt_in = |name: &str| (name == "DISABLE_AUTOUPDATER").then(|| "0".into());
        assert!(policy.opt_out(&opt_in, true).is_none());
        let no_traffic =
            |name: &str| (name == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").then(|| "0".into());
        assert!(policy.opt_out(&no_traffic, true).is_some());
        assert!(policy.opt_out(&no_traffic, false).is_none());
        let no_updates = |name: &str| (name == "DISABLE_UPDATES").then(|| "1".into());
        assert!(policy.opt_out(&no_updates, true).is_some());
        assert!(policy.opt_out(&no_updates, false).is_some());
    }

    #[tokio::test]
    async fn automatic_checks_keep_newer_claude_on_both_channels() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let bin = home.join(".local/bin");
        let versions = home.join(".local/share/claude/versions");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&versions).unwrap();
        let installed = versions.join("2.0.0");
        let launcher = bin.join("claude");
        write_executable(&installed, "#!/bin/sh\nexit 1\n");
        symlink(&installed, &launcher).unwrap();
        write_executable(&bin.join("curl"), "#!/bin/sh\necho 1.0.0\n");

        let mut status = fixture_status(Provider::Claude, &launcher, ProviderInstallMethod::Native);
        status.version = Some("2.0.0".into());
        let path = fixture_path(&bin);
        let context = CommandContext {
            search_path: Some(&path),
        };
        for (channel, minimum, reason) in [
            ("stable", None, "selected older release 1.0.0"),
            ("stable", Some("1.0.0"), "selected older release 1.0.0"),
            ("latest", None, "selected older release 1.0.0"),
            ("stable", Some("1.1.0"), "exclude release 1.0.0"),
        ] {
            let policy = Policy {
                global: Value::Null,
                user: serde_json::json!({
                    "autoUpdatesChannel": channel,
                    "minimumVersion": minimum,
                }),
                managed: vec![],
            };
            assert!(matches!(
                prepare_for_home(&status, &context, &home, &policy, true).await.unwrap(),
                Preparation::Skipped(message) if message.contains(reason)
            ));
        }
        assert_eq!(launcher.canonicalize().unwrap(), installed);
        assert!(!versions.join("1.0.0").exists());
    }

    #[tokio::test]
    async fn prepares_in_a_private_home_then_switches_the_native_launcher() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let versions = home.join(".local/share/claude/versions");
        fs::create_dir_all(&versions).unwrap();
        let previous = versions.join("1.0.0");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let launcher = bin.join("claude");
        symlink(&previous, &launcher).unwrap();
        write_executable(
            &previous,
            r#"#!/bin/sh
case "$1" in
    --version) echo '1.0.0 (Claude Code)' ;;
    install)
        test "$2" = 2.0.0 || exit 2
        versions="$XDG_DATA_HOME/claude/versions"
        bin="$HOME/.local/bin"
        mkdir -p "$versions" "$bin"
        binary="$versions/$2"
        printf '#!/bin/sh\necho "2.0.0 (Claude Code)"\n' > "$binary"
        chmod +x "$binary"
        ln -s "$binary" "$bin/claude"
        ;;
    *) exit 1 ;;
esac
"#,
        );
        write_executable(&bin.join("curl"), "#!/bin/sh\necho 2.0.0\n");

        let mut status = fixture_status(Provider::Claude, &launcher, ProviderInstallMethod::Native);
        status.version = Some("1.0.0".into());
        let path = fixture_path(&bin);
        let context = CommandContext {
            search_path: Some(&path),
        };
        let policy = Policy {
            global: Value::Null,
            user: serde_json::json!({"autoUpdatesChannel": "stable", "minimumVersion": "2.0.0"}),
            managed: vec![],
        };
        let Preparation::Ready(mut update) =
            prepare_for_home(&status, &context, home, &policy, true)
                .await
                .unwrap()
        else {
            panic!("expected staged Claude update");
        };
        assert_eq!(launcher.canonicalize().unwrap(), previous);
        assert!(!versions.join("2.0.0").exists());
        assert!(matches!(
            update.apply(Some(&path)).await.unwrap(),
            Applied::Updated(version) if version == "2.0.0"
        ));
        assert_eq!(launcher.canonicalize().unwrap(), versions.join("2.0.0"));
    }
}
