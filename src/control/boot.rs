//! `cagents daemon boot`: start the daemon when the computer boots, before
//! and without a graphical login, through a systemd user unit.
//!
//! The graphical-login autostart entry the installer registers starts the
//! daemon only once someone logs in to a desktop. A machine reached over SSH,
//! or one that rebooted with no one at its screen, never runs it, so its
//! sessions wait for a window. The unit runs the same `cagents daemon start`
//! at boot. The autostart entry, a window, and any other `daemon start` then
//! find the daemon running and start nothing: the daemon's lock admits one.
//!
//! The user manager runs at boot only for a user with lingering enabled, so
//! `enable` says when it is not and how to turn it on.
use super::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The unit's name, beside the application ID its autostart entry uses.
const UNIT: &str = "dev.cagents.Cagents.Daemon.service";

#[derive(Debug, Clone, Subcommand)]
pub enum BootCommand {
    /// Install and enable the systemd user unit that starts the daemon at boot.
    Enable,
    /// Disable and remove that unit; graphical-login autostart is unchanged.
    Disable,
    /// Report whether the daemon starts at boot, and what keeps it from doing so.
    Status,
}

pub(super) fn execute(command: BootCommand) -> Result<CommandOutput, AppError> {
    if !cfg!(target_os = "linux") {
        return Err(AppError::new(
            "daemon_boot",
            "Starting the daemon at boot uses a systemd user unit, which only Linux has",
        ));
    }
    if crate::instance::IS_DEVELOPMENT {
        return Err(AppError::new(
            "daemon_boot",
            "A development build's daemon starts with the development runner; install a release to start it at boot",
        ));
    }
    let unit = unit_path()?;
    match command {
        BootCommand::Enable => enable(&unit),
        BootCommand::Disable => disable(&unit),
        BootCommand::Status => Ok(status_output(&unit, &read_status(&unit)?)),
    }
}

/// What decides whether the daemon starts at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Status {
    /// A unit this command wrote is where the user manager reads it.
    installed: bool,
    /// A unit of the same name that this command did not write is there.
    foreign: bool,
    /// The user manager starts it with `default.target`.
    enabled: bool,
    /// The user manager itself starts at boot, before any login.
    linger: Option<bool>,
    /// The `cagents` the unit runs.
    program: Option<PathBuf>,
}

/// The unit's first line, by which `disable` and the uninstallers know a unit
/// file of this name is one this command wrote. install.sh and
/// packaging/cagents-uninstall compare against the same text.
const HEADER: &str =
    "# Installed by `cagents daemon boot enable`; `cagents daemon boot disable` removes it.";

/// What sits at the unit's path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitFile {
    Absent,
    /// A unit this command wrote, with its contents.
    Ours(String),
    /// Anything else: another unit by the same name, or a link to one.
    Foreign,
}

fn unit_file(unit: &Path) -> Result<UnitFile, AppError> {
    let metadata = match std::fs::symlink_metadata(unit) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(UnitFile::Absent),
        Err(error) => {
            return Err(AppError::new(
                "daemon_boot",
                format!("Cannot read {}: {error}", unit.display()),
            ));
        }
    };
    if !metadata.is_file() {
        return Ok(UnitFile::Foreign);
    }
    let contents = std::fs::read_to_string(unit).map_err(|error| {
        AppError::new(
            "daemon_boot",
            format!("Cannot read {}: {error}", unit.display()),
        )
    })?;
    Ok(classify(contents))
}

fn classify(contents: String) -> UnitFile {
    if contents.lines().next() == Some(HEADER) {
        UnitFile::Ours(contents)
    } else {
        UnitFile::Foreign
    }
}

fn foreign(unit: &Path) -> AppError {
    AppError::new(
        "daemon_boot",
        format!(
            "{} was not written by `cagents daemon boot enable`, so it is left as it is; move it aside to let cagents manage starting at boot",
            unit.display()
        ),
    )
}

fn enable(unit: &Path) -> Result<CommandOutput, AppError> {
    let previous = match unit_file(unit)? {
        UnitFile::Foreign => return Err(foreign(unit)),
        UnitFile::Ours(contents) => Some(contents),
        UnitFile::Absent => None,
    };
    let program = stable_program()?;
    let contents = render(&program)?;
    write_unit(unit, &contents)?;
    if let Err(error) = systemctl(&["daemon-reload"]).and_then(|()| check_unit_is_read(unit)) {
        // Leave things as they were, rather than a unit nothing reads.
        match &previous {
            Some(previous) => drop(write_unit(unit, previous)),
            None => drop(std::fs::remove_file(unit)),
        }
        let _ = systemctl(&["daemon-reload"]);
        return Err(error);
    }
    systemctl(&["enable", UNIT]).map_err(|error| {
        AppError::new(
            "daemon_boot",
            format!(
                "{} is installed but not enabled: {}",
                unit.display(),
                error.message
            ),
        )
    })?;
    Ok(status_output(unit, &read_status(unit)?))
}

fn write_unit(unit: &Path, contents: &str) -> Result<(), AppError> {
    let parent = unit.parent().expect("a unit path has a directory");
    std::fs::create_dir_all(parent).map_err(|error| {
        AppError::new(
            "daemon_boot",
            format!("Cannot create {}: {error}", parent.display()),
        )
    })?;
    // Written whole, then renamed into place, so the user manager never reads
    // half a unit.
    let staged = parent.join(format!(".{UNIT}.{}", std::process::id()));
    std::fs::write(&staged, contents)
        .and_then(|()| std::fs::rename(&staged, unit))
        .map_err(|error| {
            let _ = std::fs::remove_file(&staged);
            AppError::new(
                "daemon_boot",
                format!("Cannot write {}: {error}", unit.display()),
            )
        })
}

/// The user manager loads the unit from `unit`, not another directory: a
/// custom `XDG_CONFIG_HOME` in this shell can name one it never reads, and a
/// unit of the same name elsewhere would be the one `enable` enabled.
fn check_unit_is_read(unit: &Path) -> Result<(), AppError> {
    let loaded = systemctl_output(&["show", "--property=FragmentPath", "--value", UNIT])?;
    let loaded = loaded.trim();
    let same = !loaded.is_empty()
        && std::fs::canonicalize(loaded).ok() == std::fs::canonicalize(unit).ok();
    if same {
        return Ok(());
    }
    let directory = unit.parent().expect("a unit path has a directory");
    Err(AppError::new(
        "daemon_boot",
        if loaded.is_empty() {
            format!(
                "Your systemd user manager does not read units from {}, so it could not start the daemon at boot. It reads ~/.config/systemd/user unless its own XDG_CONFIG_HOME says otherwise; run this with the XDG_CONFIG_HOME it uses",
                directory.display()
            )
        } else {
            format!(
                "Your systemd user manager loads {UNIT} from {loaded}, not {}; remove that unit first",
                unit.display()
            )
        },
    ))
}

fn disable(unit: &Path) -> Result<CommandOutput, AppError> {
    let mut warnings = Vec::new();
    match unit_file(unit)? {
        UnitFile::Foreign => return Err(foreign(unit)),
        UnitFile::Absent => {}
        UnitFile::Ours(_) => {
            // Disabling stops nothing: the unit only ever started the daemon,
            // which keeps running with its sessions until `cagents daemon stop`.
            // Without a user manager to ask, as under `sudo` or `su`, the
            // files go anyway: its link is what `systemctl disable` removes.
            if let Err(error) = systemctl(&["disable", UNIT]) {
                warnings.push(format!(
                    "{}; removed the unit's files directly, which the user manager reads at its next start",
                    error.message
                ));
                let wants = unit
                    .parent()
                    .expect("a unit path has a directory")
                    .join("default.target.wants")
                    .join(UNIT);
                if std::fs::symlink_metadata(&wants).is_ok_and(|metadata| metadata.is_symlink())
                    && let Err(error) = std::fs::remove_file(&wants)
                {
                    warnings.push(format!("Cannot remove {}: {error}", wants.display()));
                }
            }
            std::fs::remove_file(unit).map_err(|error| {
                AppError::new(
                    "daemon_boot",
                    format!("Cannot remove {}: {error}", unit.display()),
                )
            })?;
            if warnings.is_empty()
                && let Err(error) = systemctl(&["daemon-reload"])
            {
                warnings.push(error.message);
            }
        }
    }
    // A daemon started later in this boot is no longer one the unit started.
    match crate::instance::daemon_started_at_boot_directory() {
        Ok(directory) => {
            let record = crate::instance::daemon_started_at_boot_record(&directory);
            match std::fs::remove_file(&record) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warnings.push(format!("Cannot remove {}: {error}", record.display())),
            }
        }
        Err(error) => warnings.push(format!(
            "Cannot find the daemon's runtime directory ({error}), so a daemon an update starts later in this boot may still take its session variables from the user manager"
        )),
    }
    let mut output = status_output(unit, &read_status(unit)?);
    if !warnings.is_empty() {
        output.data["warnings"] = json!(warnings);
        output.human = format!("{}\n{}", warnings.join("\n"), output.human);
    }
    Ok(output)
}

fn read_status(unit: &Path) -> Result<Status, AppError> {
    let file = unit_file(unit)?;
    let enabled = matches!(file, UnitFile::Ours(_))
        && Command::new("systemctl")
            .args(["--user", "is-enabled", "--quiet", UNIT])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    Ok(Status {
        program: match &file {
            UnitFile::Ours(contents) => program_of(contents),
            _ => None,
        },
        installed: matches!(file, UnitFile::Ours(_)),
        foreign: file == UnitFile::Foreign,
        enabled,
        linger: linger(),
    })
}

fn status_output(unit: &Path, status: &Status) -> CommandOutput {
    let starts_at_boot = status.enabled
        && status.linger == Some(true)
        && status.program.as_deref().is_some_and(Path::exists);
    CommandOutput {
        command: "daemon.boot",
        data: json!({
            "starts_at_boot": starts_at_boot,
            "unit": unit,
            "installed": status.installed,
            "foreign_unit": status.foreign,
            "enabled": status.enabled,
            "linger": status.linger,
            "program": status.program,
        }),
        human: human(unit, status),
    }
}

fn human(unit: &Path, status: &Status) -> String {
    if status.foreign {
        return format!(
            "A unit cagents did not write is at {}; cagents leaves it alone. The daemon starts at a graphical login.",
            unit.display()
        );
    }
    if !status.installed {
        return "The daemon starts at a graphical login, not at boot. `cagents daemon boot enable` starts it at boot."
            .to_owned();
    }
    let mut lines = vec![match (status.enabled, status.linger) {
        (true, Some(true)) => "The daemon starts at boot, before anyone logs in.".to_owned(),
        (true, Some(false)) => {
            "The daemon's boot unit is enabled, but it starts only once you log in.".to_owned()
        }
        (true, None) => {
            "The daemon's boot unit is enabled; whether it starts before anyone logs in is unknown, because loginctl did not say whether lingering is on."
                .to_owned()
        }
        (false, _) => format!(
            "The daemon's boot unit is installed but not enabled; `systemctl --user enable {UNIT}` enables it."
        ),
    }];
    match status.linger {
        Some(true) => {}
        Some(false) => lines.push(
            "Your systemd user manager starts at boot only with lingering on: run `loginctl enable-linger` (your distribution may ask for an administrator)."
                .to_owned(),
        ),
        None => lines.push(
            "`loginctl show-user` reports whether lingering is on; `loginctl enable-linger` turns it on."
                .to_owned(),
        ),
    }
    if let Some(program) = &status.program
        && !program.exists()
    {
        lines.push(format!(
            "It runs {}, which no longer exists, so systemd skips it; `cagents daemon boot enable` points it at this installation.",
            program.display()
        ));
    }
    lines.join("\n")
}

/// The unit that runs `program daemon start` at boot.
///
/// `daemon start` returns once the daemon serves, leaving the daemon running
/// in its own session, so the unit is a oneshot that stays active. Stopping
/// or disabling the unit kills nothing: only its main process, which has
/// already exited, is in its kill set, and the daemon and its sessions stop
/// only with `cagents daemon stop` or the machine.
///
/// `daemon start` bounds its own wait, which an interrupted update's recovery
/// lengthens with each session it restores, so systemd adds no second limit.
/// A removed package or installation skips the unit rather than failing it.
fn render(program: &Path) -> Result<String, AppError> {
    let Some(text) = program.to_str().filter(|text| !text.contains(['\n', '\r'])) else {
        return Err(AppError::new(
            "daemon_boot",
            format!(
                "A unit cannot run {}: its path is not UTF-8 or has a line break",
                program.display()
            ),
        ));
    };
    Ok(format!(
        "\
{HEADER}
[Unit]
Description=cagents daemon
Documentation=https://github.com/CarsonBurke/cagents/blob/main/docs/daemon.md
ConditionFileIsExecutable={condition}

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={program} daemon start
# The daemon then takes a graphical login's display and agent variables from
# the user manager for each terminal, rather than keeping these from boot.
Environment={at_boot}=1
KillMode=process
TimeoutStartSec=infinity

[Install]
WantedBy=default.target
",
        condition = text.replace('%', "%%"),
        program = systemd_quoted(text),
        at_boot = crate::instance::DAEMON_AT_BOOT_ENV,
    ))
}

/// `text`, a path, as one systemd command-line word.
fn systemd_quoted(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        return text.to_owned();
    }
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The program a unit this command wrote runs.
fn program_of(unit: &str) -> Option<PathBuf> {
    let line = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    let program = line.strip_suffix(" daemon start")?;
    let program = program
        .strip_prefix('"')
        .and_then(|quoted| quoted.strip_suffix('"'))
        .map(|quoted| {
            quoted
                .replace("$$", "$")
                .replace("%%", "%")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        })
        .unwrap_or_else(|| program.to_owned());
    Some(PathBuf::from(program))
}

/// The `cagents` the unit should run: a name for this executable that an
/// update keeps pointing at the installed version, such as an installation's
/// `current/bin/cagents` or a package's `/usr/bin/cagents`, rather than this
/// version's own file, which a later update removes.
fn stable_program() -> Result<PathBuf, AppError> {
    let current = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| {
            AppError::new("daemon_boot", format!("Cannot find this cagents: {error}"))
        })?;
    let candidates = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join("cagents"))
        .collect::<Vec<_>>();
    choose_program(
        &current,
        &candidates,
        |path| std::fs::canonicalize(path).ok(),
        |root| root.join(INSTALL_ROOT_MARKER).is_file(),
    )
    .ok_or_else(|| {
        AppError::new(
            "daemon_boot",
            format!(
                "Neither this installation's `current` link nor a `cagents` on your PATH runs this cagents ({}), so a unit could not survive an update. Run the installed `cagents daemon boot enable`",
                current.display()
            ),
        )
    })
}

/// The file install.sh keeps at the root of each installation, beside
/// `versions/` and the `current` link to the version it runs.
const INSTALL_ROOT_MARKER: &str = ".cagents-install-root";

/// The installation root holding `current` when it is
/// `ROOT/versions/VERSION/bin/cagents` and `ROOT` carries the marker.
fn installation_root(current: &Path, is_root: impl Fn(&Path) -> bool) -> Option<&Path> {
    let bin = current.parent()?;
    let version = bin.parent()?;
    let versions = version.parent()?;
    let root = versions.parent()?;
    (bin.file_name()? == "bin" && versions.file_name()? == "versions" && is_root(root))
        .then_some(root)
}

/// The stable name for `current`, this executable's real path:
/// - within an installation, its `current/bin/cagents`, which every update
///   repoints, whatever prefix it was installed to;
/// - otherwise the first absolute `candidate` that is `current` by another
///   name, as a package's `/usr/bin/cagents` link is;
/// - otherwise `current` itself, unless it is inside an installation, whose
///   versions an update removes.
fn choose_program(
    current: &Path,
    candidates: &[PathBuf],
    resolve: impl Fn(&Path) -> Option<PathBuf>,
    is_root: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let root = installation_root(current, is_root);
    root.map(|root| root.join("current/bin/cagents"))
        .into_iter()
        .chain(
            candidates
                .iter()
                .filter(|candidate| candidate.is_absolute())
                .cloned(),
        )
        .find(|candidate| resolve(candidate).as_deref() == Some(current))
        .or_else(|| root.is_none().then(|| current.to_owned()))
}

/// Where the user manager reads a user's own units.
fn unit_path() -> Result<PathBuf, AppError> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| AppError::new("daemon_boot", "Neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(config.join("systemd/user").join(UNIT))
}

/// Whether this user's manager runs from boot, when `loginctl` says.
fn linger() -> Option<bool> {
    let output = Command::new("loginctl")
        .args(["show-user", &user_id(), "--property=Linger", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success());
    match output {
        Some(output) => parse_linger(&String::from_utf8_lossy(&output.stdout)),
        // logind knows no user without a session or lingering; its record of
        // who lingers is a file per user name.
        None => {
            let name = std::env::var("USER").ok().filter(|name| !name.is_empty())?;
            let directory = Path::new("/var/lib/systemd/linger");
            directory.is_dir().then(|| directory.join(name).exists())
        }
    }
}

fn parse_linger(value: &str) -> Option<bool> {
    match value.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn user_id() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }.to_string()
}

fn systemctl(arguments: &[&str]) -> Result<(), AppError> {
    systemctl_output(arguments).map(drop)
}

fn systemctl_output(arguments: &[&str]) -> Result<String, AppError> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            AppError::new(
                "daemon_boot",
                format!(
                    "Cannot run systemctl: {error}; starting at boot needs a systemd user manager"
                ),
            )
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(AppError::new(
        "daemon_boot",
        format!(
            "`systemctl --user {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    fn the_unit_starts_the_daemon_once_and_never_stops_it() {
        let unit = render(Path::new(
            "/home/someone/.local/share/cagents-install/current/bin/cagents",
        ))
        .unwrap();
        assert_eq!(unit.lines().next(), Some(HEADER));
        assert!(
            unit.contains(
                "ExecStart=/home/someone/.local/share/cagents-install/current/bin/cagents daemon start\n"
            ),
            "{unit}"
        );
        for line in [
            "ConditionFileIsExecutable=/home/someone/.local/share/cagents-install/current/bin/cagents",
            "Type=oneshot",
            "RemainAfterExit=yes",
            "KillMode=process",
            // `daemon start` bounds its own wait, which recovery lengthens.
            "TimeoutStartSec=infinity",
            "WantedBy=default.target",
            "Environment=CAGENTS_DAEMON_AT_BOOT=1",
        ] {
            assert!(unit.lines().any(|l| l == line), "{line} in {unit}");
        }
        assert_eq!(
            program_of(&unit).as_deref(),
            Some(Path::new(
                "/home/someone/.local/share/cagents-install/current/bin/cagents"
            ))
        );
        assert!(matches!(classify(unit), UnitFile::Ours(_)));
    }

    #[test]
    fn a_path_systemd_would_split_or_expand_is_quoted_and_read_back() {
        let program = Path::new("/home/some one/100%/$HOME/cagents");
        let unit = render(program).unwrap();
        assert!(
            unit.contains(r#"ExecStart="/home/some one/100%%/$$HOME/cagents" daemon start"#),
            "{unit}"
        );
        // Conditions expand specifiers but not variables, and take the rest
        // of the line as the path.
        assert!(
            unit.contains("ConditionFileIsExecutable=/home/some one/100%%/$HOME/cagents\n"),
            "{unit}"
        );
        assert_eq!(program_of(&unit).as_deref(), Some(program));
        assert!(render(Path::new("/home/u/a\nb/cagents")).is_err());
        use std::os::unix::ffi::OsStrExt;
        assert!(
            render(Path::new(std::ffi::OsStr::from_bytes(
                b"/home/\xff/cagents"
            )))
            .is_err()
        );
    }

    #[test]
    fn only_a_unit_this_command_wrote_is_replaced_or_removed() {
        assert!(matches!(
            classify(format!("{HEADER}\n[Unit]\n")),
            UnitFile::Ours(_)
        ));
        assert_eq!(
            classify("[Unit]\nDescription=my own\n".to_owned()),
            UnitFile::Foreign
        );
        // The header must be the first line, as the uninstallers check it.
        assert_eq!(classify(format!("# Mine.\n{HEADER}\n")), UnitFile::Foreign);
        let directory = tempfile::tempdir().unwrap();
        let unit = directory.path().join(UNIT);
        assert_eq!(unit_file(&unit).unwrap(), UnitFile::Absent);
        std::fs::write(directory.path().join("elsewhere"), format!("{HEADER}\n")).unwrap();
        std::os::unix::fs::symlink(directory.path().join("elsewhere"), &unit).unwrap();
        // A link, as `systemctl --user link` makes, is someone else's.
        assert_eq!(unit_file(&unit).unwrap(), UnitFile::Foreign);
        assert!(enable(&unit).is_err());
        assert!(disable(&unit).is_err());
        assert!(unit.is_symlink());
    }

    #[test]
    fn an_installation_runs_its_current_link_whatever_its_prefix() {
        let current = Path::new("/opt/tools/cagents/versions/0.2.4/bin/cagents");
        let stable = PathBuf::from("/opt/tools/cagents/current/bin/cagents");
        let link = PathBuf::from("/home/u/.local/bin/cagents");
        let is_root = |root: &Path| root == Path::new("/opt/tools/cagents");
        let resolve = |path: &Path| match path.to_str() {
            Some("/opt/tools/cagents/current/bin/cagents" | "/home/u/.local/bin/cagents") => {
                Some(current.to_owned())
            }
            Some("/usr/bin/cagents") => Some(PathBuf::from("/usr/lib/cagents/bin/cagents")),
            _ => None,
        };
        assert_eq!(
            choose_program(current, &[link.clone()], resolve, is_root),
            Some(stable)
        );
        // Not the version `current` points at: only another name will do,
        // never this version's own file, which a later update removes.
        let older = Path::new("/opt/tools/cagents/versions/0.2.3/bin/cagents");
        let resolve_older = |path: &Path| (path == link).then(|| older.to_owned());
        assert_eq!(
            choose_program(older, &[link.clone()], resolve_older, is_root),
            Some(link)
        );
        assert_eq!(
            choose_program(
                older,
                &[PathBuf::from("/usr/bin/cagents")],
                resolve,
                is_root
            ),
            None
        );
        // Without the marker, a `versions` directory is not an installation.
        let built = Path::new("/src/versions/x/bin/cagents");
        assert_eq!(
            choose_program(built, &[], |_| None, |_| false),
            Some(built.to_owned())
        );
    }

    #[test]
    fn a_package_runs_the_name_on_path() {
        let packaged = Path::new("/usr/lib/cagents/bin/cagents");
        let resolve =
            |path: &Path| (path == Path::new("/usr/bin/cagents")).then(|| packaged.to_owned());
        assert_eq!(
            choose_program(
                packaged,
                &[
                    PathBuf::from("relative/cagents"),
                    PathBuf::from("/usr/bin/cagents")
                ],
                resolve,
                |_| false
            ),
            Some(PathBuf::from("/usr/bin/cagents"))
        );
        let direct = Path::new("/usr/bin/cagents");
        assert_eq!(
            choose_program(direct, &[], |_| None, |_| false),
            Some(direct.to_owned())
        );
    }

    #[test]
    fn status_says_what_keeps_the_daemon_from_starting_at_boot() {
        let unit = Path::new("/home/u/.config/systemd/user").join(UNIT);
        let absent = Status {
            installed: false,
            foreign: false,
            enabled: false,
            linger: Some(true),
            program: None,
        };
        assert!(human(&unit, &absent).contains("`cagents daemon boot enable`"));
        let foreign = Status {
            foreign: true,
            ..absent.clone()
        };
        assert!(human(&unit, &foreign).contains("did not write"));
        let lingerless = Status {
            installed: true,
            foreign: false,
            enabled: true,
            linger: Some(false),
            program: None,
        };
        let text = human(&unit, &lingerless);
        assert!(text.contains("starts only once you log in"), "{text}");
        assert!(text.contains("loginctl enable-linger"), "{text}");
        let unknown = Status {
            linger: None,
            ..lingerless.clone()
        };
        let text = human(&unit, &unknown);
        assert!(text.contains("unknown"), "{text}");
        assert!(!text.contains("only once you log in"), "{text}");
        let ready = Status {
            linger: Some(true),
            ..lingerless
        };
        assert_eq!(
            human(&unit, &ready),
            "The daemon starts at boot, before anyone logs in."
        );
    }

    #[test]
    fn linger_is_unknown_unless_loginctl_says() {
        assert_eq!(parse_linger("yes\n"), Some(true));
        assert_eq!(parse_linger("no"), Some(false));
        assert_eq!(parse_linger(""), None);
        assert_eq!(parse_linger("maybe"), None);
    }
}
