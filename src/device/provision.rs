//! `cagents device update`: bring a saved device to the build this computer
//! runs, so its windows and commands can reach the device's daemon again.
//!
//! The window and the daemon must speak the same protocol. Linux receives
//! the desktop's installed build over SSH. Apple Silicon uses its own
//! verified, matching release and runs its installed updater over SSH.
//! Both routes use the device's update review. See docs/devices.md#updating-a-device.

use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use sha2::{Digest, Sha256};

use super::{Device, control_path, quote, remote_executable, ssh};
use crate::terminal_ipc::PROTOCOL_VERSION;

/// The installer of this very build, which knows its own archive layout.
const INSTALLER: &str = include_str!("../../install.sh");
/// The installer's marker on a tree staged by `--stage-for-upgrade`, holding
/// the version it was staged beside.
const STAGED_MARKER: &str = ".cagents-local-candidate-v1";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const LINUX_BUILD: bool = cfg!(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu"
));
/// zlib level for the archive. Level 1 compresses this tree at several
/// hundred MB/s to within a few percent of level 6, which needs seconds more
/// than it saves on a local network.
const COMPRESSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Accept the device's update review without a prompt.
    pub yes: bool,
    /// Permit active native turns on the device to stop.
    pub interrupt: bool,
}

/// What `update` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The device's daemon already speaks this computer's protocol, or it is
    /// stopped and its CLI would start one that does (`running` is false).
    AlreadyCompatible { running: bool },
    /// The device now runs this computer's build. `sent` is the archive's
    /// size when it had to be uploaded.
    Updated { sent: Option<u64> },
    /// The device had no cagents; it now has this computer's build, and its
    /// daemon starts with the first connection.
    Installed { sent: u64 },
}

/// Progress, for a person waiting on a transfer that can take a minute.
#[derive(Debug)]
pub enum Step<'a> {
    Inspecting,
    Sending {
        read: u64,
        total: u64,
    },
    Installing,
    Upgrading,
    Verifying,
    /// A note the person should act on, such as a PATH that lacks cagents.
    Note(&'a str),
}

/// The device's daemon, as seen through its bridge without starting it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Daemon {
    Compatible,
    Speaks(u16),
    Stopped,
    /// It could not be reached for another reason; the device's own update
    /// review knows its daemon better than this computer does.
    Unknown(String),
}

/// What [`update`] would do, found the same way and without changing the
/// device.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Compatible,
    /// It has an older cagents; its daemon, if it answered, speaks `protocol`.
    Update {
        protocol: Option<u16>,
    },
    /// It has no cagents.
    Install,
    /// It has a newer cagents, which is never replaced.
    Newer {
        protocol: u16,
    },
}

/// Like [`update`], `device` must be the selected device. Also returns the
/// note [`update`] would give, such as an executable SSH commands cannot find.
pub fn check(device: &Device) -> io::Result<(Check, Option<String>)> {
    let local = Local::current()?;
    let session = Session::open(device)?;
    let remote = Remote::probe(&session, device, &local.version)?;
    let daemon = daemon_state();
    if let Some(protocol) = newer(&remote, &daemon) {
        return Ok((Check::Newer { protocol }, None));
    }
    let plan = plan(device, &local, &remote, &daemon)?;
    verify_mac_release(device, &local.version, &plan.action)?;
    let check = match plan.action {
        Action::Nothing => Check::Compatible,
        Action::Install { upgrade: false, .. } => Check::Install,
        Action::Install { .. } | Action::Upgrade { .. } | Action::MacRelease { .. } => {
            Check::Update {
                protocol: match daemon {
                    Daemon::Speaks(protocol) => Some(protocol),
                    _ => remote.protocol,
                },
            }
        }
        Action::MacInstaller => unreachable!("Installer requirement was reported by verification"),
    };
    Ok((check, plan.note))
}

/// Brings `device` to the build this executable belongs to. `device` must be
/// the selected device ([`super::select`]).
pub fn update(
    device: &Device,
    options: Options,
    progress: &mut dyn FnMut(Step),
) -> io::Result<Outcome> {
    let local = Local::current()?;
    progress(Step::Inspecting);
    let session = Session::open(device)?;
    let remote = Remote::probe(&session, device, &local.version)?;
    let daemon = daemon_state();
    let plan = plan(device, &local, &remote, &daemon)?;
    let mac_release = verify_mac_release(device, &local.version, &plan.action)?;
    if let Some(note) = &plan.note {
        progress(Step::Note(note));
    }
    let hands_over = matches!(
        plan.action,
        Action::Upgrade { .. } | Action::MacRelease { .. } | Action::Install { upgrade: true, .. }
    );
    // Refuse before sending anything when nobody can answer the review.
    if hands_over && !options.yes && !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(io::Error::other(format!(
            "{name}'s update needs a decision about its running work. Run `cagents device update {name}` in a terminal to review it, or pass --yes to accept the safe choice: terminals continue, other conversations wait until idle, and anything else that would stop blocks the update.",
            name = device.name
        )));
    }
    let sent = match &plan.action {
        Action::Nothing => {
            return Ok(Outcome::AlreadyCompatible {
                running: daemon == Daemon::Compatible,
            });
        }
        Action::Upgrade { .. } | Action::MacRelease { .. } => None,
        Action::MacInstaller => unreachable!("Installer requirement was reported by verification"),
        Action::Install { prefix, .. } => {
            let sent = session.upload_and_stage(&local, prefix, progress)?;
            Some(sent)
        }
    };
    let cli = match plan.action {
        Action::Install { upgrade: false, .. } => {
            return Ok(Outcome::Installed {
                sent: sent.unwrap_or_default(),
            });
        }
        Action::Install { prefix, .. } => {
            format!("{prefix}/versions/{}/bin/cagents", local.version)
        }
        Action::Upgrade { cli } | Action::MacRelease { cli } => cli,
        Action::Nothing | Action::MacInstaller => unreachable!("returned above"),
    };
    progress(Step::Upgrading);
    let release = mac_release
        .as_deref()
        .map(|digest| (local.version.as_str(), digest));
    session.upgrade(device, &cli, release, options)?;
    progress(Step::Verifying);
    match daemon_state() {
        Daemon::Stopped if release.is_some() => {
            let selected = Remote::probe(&session, device, &local.version)?;
            if selected.protocol == Some(PROTOCOL_VERSION)
                && selected
                    .installation
                    .and_then(|found| found.current)
                    .as_deref()
                    == Some(&local.version)
            {
                Ok(Outcome::Updated { sent })
            } else {
                Err(io::Error::other(format!(
                    "Could not verify the installed update on {}",
                    device.name
                )))
            }
        }
        Daemon::Compatible | Daemon::Stopped => Ok(Outcome::Updated { sent }),
        Daemon::Speaks(actual) => Err(io::Error::other(format!(
            "{name}'s update finished, but its daemon still speaks protocol {actual}, not {PROTOCOL_VERSION}. Run `cagents --device {name} daemon status` to see what it runs.",
            name = device.name
        ))),
        Daemon::Unknown(error) => Err(io::Error::other(format!(
            "{name}'s update finished, but this computer could not reach its daemon: {error}",
            name = device.name
        ))),
    }
}

fn verify_mac_release(
    device: &Device,
    version: &str,
    action: &Action,
) -> io::Result<Option<String>> {
    if !matches!(action, Action::MacRelease { .. } | Action::MacInstaller) {
        return Ok(None);
    }
    let digest = crate::update::macos_device_release_digest(version).map_err(|error| {
        io::Error::other(format!(
            "Could not verify the Apple Silicon release for {}: {error}",
            device.name
        ))
    })?;
    if *action == Action::MacInstaller {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "Install cagents {version} on {} with the macOS installer before reconnecting. Run this on the Mac:

{}",
                device.name,
                crate::update::installer_command(Some(version)),
            ),
        ));
    }
    Ok(Some(digest))
}

/// The installed build this executable belongs to.
struct Local {
    version: String,
    root: PathBuf,
}

impl Local {
    fn current() -> io::Result<Self> {
        if !(LINUX_BUILD || cfg!(all(target_os = "macos", target_arch = "aarch64"))) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Device updates need an installed cagents build for x86-64 Linux or Apple Silicon macOS",
            ));
        }
        let executable = crate::process::current_executable()?;
        let layout = crate::update::locate(&executable).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "This cagents ({}) is not an installed build, so it has nothing to send to a device. Install cagents for this computer, then run its installed CLI.",
                    executable.display()
                ),
            )
        })?;
        let version = crate::version().to_owned();
        // A build copied into another version's tree must not pass for it.
        if layout.version_root.file_name() != Some(version.as_ref()) {
            return Err(io::Error::other(format!(
                "this executable reports version {version} but is installed as {}",
                layout.version_root.display()
            )));
        }
        Ok(Self {
            version,
            root: layout.version_root,
        })
    }

    /// The newest glibc symbol version any executable in this build needs,
    /// with the executable that needs it.
    fn required_glibc(&self) -> io::Result<Option<(Vec<u32>, String)>> {
        let mut newest: Option<(Vec<u32>, String)> = None;
        for entry in fs::read_dir(self.root.join("bin"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            for version in glibc_versions(&entry.path())? {
                if newest.as_ref().is_none_or(|(known, _)| version > *known) {
                    newest = Some((version, name.clone()));
                }
            }
        }
        Ok(newest)
    }
}

/// The `GLIBC_x.y` versions an ELF executable needs, read from its version
/// requirements without loading the whole file. Not an ELF file: none.
fn glibc_versions(path: &Path) -> io::Result<Vec<Vec<u32>>> {
    use object::read::elf::ElfFile64;
    let file = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    if (&file).read_exact(&mut magic).is_err() || magic != *b"\x7fELF" {
        return Ok(Vec::new());
    }
    let cache = object::ReadCache::new(file);
    let invalid = |error: object::Error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    };
    let elf = ElfFile64::<object::Endianness, _>::parse(&cache).map_err(invalid)?;
    let endian = elf.endian();
    let sections = elf.elf_section_table();
    let Some((mut needs, link)) = sections.gnu_verneed(endian, &cache).map_err(invalid)? else {
        return Ok(Vec::new());
    };
    let strings = sections.strings(endian, &cache, link).map_err(invalid)?;
    let mut versions = Vec::new();
    while let Some((_, mut auxiliary)) = needs.next().map_err(invalid)? {
        while let Some(aux) = auxiliary.next().map_err(invalid)? {
            let name = aux.name(endian, strings).map_err(invalid)?;
            if let Some(version) = std::str::from_utf8(name)
                .ok()
                .and_then(|name| name.strip_prefix("GLIBC_"))
                .and_then(parse_version)
            {
                versions.push(version);
            }
        }
    }
    Ok(versions)
}

fn parse_version(text: &str) -> Option<Vec<u32>> {
    text.split('.').map(|part| part.parse().ok()).collect()
}

fn display_version(version: &[u32]) -> String {
    version
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// What the device's shell reports about it and its cagents installation.
#[derive(Debug, Default, PartialEq, Eq)]
struct Remote {
    os: String,
    arch: String,
    glibc: Option<Vec<u32>>,
    home: String,
    /// The CLI the device's executable resolves to, symlinks followed.
    resolved: Option<String>,
    /// The device's executable was not found; the CLI is the one at the
    /// installer's default path.
    off_path: bool,
    default_prefix: String,
    installation: Option<Installation>,
    /// The installed Mac can require a particular verified release digest.
    signed_release_updates: bool,
    /// What `daemon protocol-version` of the device's CLI printed.
    protocol: Option<u16>,
}

/// An `install.sh` installation on the device.
#[derive(Debug, Default, PartialEq, Eq)]
struct Installation {
    prefix: String,
    /// Whether the prefix carries the installer's root marker.
    marked: bool,
    /// The version `current` serves.
    current: Option<String>,
    /// Whether the version this computer runs is already installed there.
    has_wanted: bool,
    /// The version that tree was staged beside, if it was staged.
    wanted_staged_beside: Option<String>,
}

const PROBE: &str = r#"
field() { printf '%s=%s\n' "$1" "$2"; }
resolve_macos_cli() {
    file=$1
    case $file in /*) ;; *) file=$PWD/$file ;; esac
    remaining=40
    while [ "$remaining" -gt 0 ]; do
        directory=$(CDPATH= cd -P "${file%/*}/" && pwd -P) || return 1
        file=${directory%/}/${file##*/}
        if [ ! -L "$file" ]; then
            [ -x "$file" ] || return 1
            printf '%s\n' "$file"
            return
        fi
        link=$(readlink "$file") || return 1
        case $link in /*) file=$link ;; *) file=$directory/$link ;; esac
        remaining=$((remaining - 1))
    done
    return 1
}
os=$(uname -s)
field os "$os"
field arch "$(uname -m)"
field home "$HOME"
case $os in
    Darwin) prefix="$HOME/Library/Application Support/cagents-install" ;;
    *)
        field glibc "$(getconf GNU_LIBC_VERSION 2>/dev/null)"
        prefix=${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install
        ;;
esac
field default_prefix "$prefix"
path=$(command -v "$exe" 2>/dev/null) || path=
if [ -z "$path" ] && [ -x "$prefix/current/bin/cagents" ]; then
    path=$prefix/current/bin/cagents
    field off_path 1
fi
resolved=
if [ -n "$path" ]; then
    case $os in
        Darwin) resolved=$(resolve_macos_cli "$path" 2>/dev/null) || resolved= ;;
        *) resolved=$(readlink -f "$path" 2>/dev/null) || resolved= ;;
    esac
fi
field resolved "$resolved"
case $resolved in
    */versions/*/bin/cagents)
        root=${resolved%/bin/cagents}
        prefix=${root%/versions/*}
        field prefix "$prefix"
        [ ! -f "$prefix/.cagents-install-root" ] || field marked 1
        current=$(readlink "$prefix/current" 2>/dev/null) || current=
        field current "${current#versions/}"
        if [ -d "$prefix/versions/$wanted" ]; then
            field has_wanted 1
            field wanted_staged_beside "$(cat "$prefix/versions/$wanted/$marker" 2>/dev/null)"
        fi
        ;;
esac
if [ -n "$resolved" ]; then
    field protocol "$("$resolved" daemon protocol-version 2>/dev/null)"
    if [ "$os" = Darwin ] && [ -f "$prefix/.cagents-install-root" ] &&
        "$prefix/current/bin/cagents" daemon upgrade --help 2>/dev/null | grep -q -e --require-release
    then
        field signed_release_updates 1
    fi
fi
"#;

impl Remote {
    fn probe(session: &Session, device: &Device, wanted: &str) -> io::Result<Self> {
        let script = format!(
            "exe={}\nwanted={}\nmarker={STAGED_MARKER}\n{PROBE}",
            remote_executable(&device.executable),
            quote(wanted)
        );
        let output = session.script(&script)?;
        if !output.status.success() {
            return Err(session.failure("inspect", &output));
        }
        Ok(Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    fn parse(text: &str) -> Self {
        let mut remote = Self::default();
        let mut installation: Option<Installation> = None;
        let some = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "os" => remote.os = value.to_owned(),
                "arch" => remote.arch = value.to_owned(),
                "glibc" => {
                    remote.glibc = value
                        .strip_prefix("glibc ")
                        .and_then(|version| parse_version(version.trim()));
                }
                "home" => remote.home = value.to_owned(),
                "default_prefix" => remote.default_prefix = value.to_owned(),
                "off_path" => remote.off_path = value == "1",
                "resolved" => remote.resolved = some(value),
                "protocol" => remote.protocol = value.trim().parse().ok(),
                "signed_release_updates" => remote.signed_release_updates = value == "1",
                "prefix" => {
                    installation = Some(Installation {
                        prefix: value.to_owned(),
                        ..Installation::default()
                    });
                }
                _ => {
                    let Some(installation) = installation.as_mut() else {
                        continue;
                    };
                    match key {
                        "marked" => installation.marked = value == "1",
                        "current" => installation.current = some(value),
                        "has_wanted" => installation.has_wanted = value == "1",
                        "wanted_staged_beside" => installation.wanted_staged_beside = some(value),
                        _ => {}
                    }
                }
            }
        }
        remote.installation = installation;
        remote
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    /// The build is already on the device; only its daemon needs handing over.
    Upgrade {
        cli: String,
    },
    /// Let the installed Mac fetch and apply a specific signed release.
    MacRelease {
        cli: String,
    },
    /// This Mac needs the supported manual installer entry point.
    MacInstaller,
    /// Send the build, stage it under `prefix`, then hand the daemon over
    /// unless there was no installation to hand over from.
    Install {
        prefix: String,
        upgrade: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct Plan {
    action: Action,
    note: Option<String>,
}

/// The version and protocol facts this computer knows about itself, apart
/// from the files it reads to learn them.
trait Build {
    fn version(&self) -> &str;
    fn can_send_linux(&self) -> bool;
    fn required_glibc(&self) -> io::Result<Option<(Vec<u32>, String)>>;
}

impl Build for Local {
    fn version(&self) -> &str {
        &self.version
    }
    fn can_send_linux(&self) -> bool {
        LINUX_BUILD
    }

    fn required_glibc(&self) -> io::Result<Option<(Vec<u32>, String)>> {
        Local::required_glibc(self)
    }
}

/// Decides what bringing the device to `local` takes, refusing anything
/// that would downgrade it or install a build it cannot run, before a single
/// byte is sent.
fn plan(device: &Device, local: &dyn Build, remote: &Remote, daemon: &Daemon) -> io::Result<Plan> {
    let name = &device.name;
    let version = local.version();
    let refuse = |message: String| Err(io::Error::new(io::ErrorKind::Unsupported, message));
    match daemon {
        Daemon::Compatible => {
            return Ok(Plan {
                action: Action::Nothing,
                note: None,
            });
        }
        Daemon::Stopped if remote.protocol == Some(PROTOCOL_VERSION) => {
            // Its daemon starts with the next connection, at this protocol.
            return Ok(Plan {
                action: Action::Nothing,
                note: None,
            });
        }
        Daemon::Unknown(error) if remote.protocol == Some(PROTOCOL_VERSION) => {
            // This build is already there; sending it again cannot help reach
            // its daemon.
            let installed_link = format!("{}/.local/bin/cagents", remote.home);
            return Ok(Plan {
                action: Action::Nothing,
                note: Some(if remote.off_path {
                    unreachable_note(device, &installed_link)
                } else {
                    format!("This computer could not reach {name}'s daemon: {error}")
                }),
            });
        }
        _ => {}
    }
    if let Some(actual) = newer(remote, daemon) {
        return refuse(format!(
            "{name} has a newer cagents (protocol {actual}) than this computer (protocol {PROTOCOL_VERSION}). A device is never downgraded; update cagents on this computer, then connect again."
        ));
    }
    if remote.os == "Darwin" && remote.arch == "arm64" {
        let installed_link = format!("{}/.local/bin/cagents", remote.home);
        let installation = remote.installation.as_ref().filter(|found| found.marked);
        if let Some(current) = installation.and_then(|found| found.current.as_deref()) {
            let selected = semver::Version::parse(current).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Could not compare the version selected on {name}: {current}"),
                )
            })?;
            let desired = semver::Version::parse(version).map_err(io::Error::other)?;
            match selected.cmp_precedence(&desired) {
                std::cmp::Ordering::Greater => {
                    return refuse(format!(
                        "{name} already selects cagents {current}. Update this computer before reconnecting."
                    ));
                }
                std::cmp::Ordering::Equal => {
                    return refuse(format!(
                        "{name} already selects cagents {current}, but the serving daemon could not be verified with it. Review the installation on the Mac before reconnecting."
                    ));
                }
                std::cmp::Ordering::Less => {}
            }
        }
        let action = match installation {
            Some(installation) if remote.signed_release_updates => Action::MacRelease {
                cli: format!("{}/current/bin/cagents", installation.prefix),
            },
            _ => Action::MacInstaller,
        };
        return Ok(Plan {
            action,
            note: remote
                .off_path
                .then(|| unreachable_note(device, &installed_link)),
        });
    }
    if remote.os != "Linux" || remote.arch != "x86_64" {
        return refuse(format!(
            "{name} is a {} {} computer. Device updates support x86-64 Linux with glibc and Apple Silicon macOS.",
            remote.os, remote.arch
        ));
    }
    if !local.can_send_linux() {
        return refuse(
            "Updating a Linux device needs an installed x86-64 Linux build on this computer".into(),
        );
    }
    let Some(theirs) = &remote.glibc else {
        return refuse(format!(
            "{name} did not report a GNU C library version; this computer's build needs glibc. Install cagents there for its own system."
        ));
    };
    if let Some((needed, executable)) = local.required_glibc()?
        && needed > *theirs
    {
        return refuse(format!(
            "This computer's build needs glibc {} ({executable}), but {name} has glibc {}. Install a build made for {name}'s system there, or build this one on an older system.",
            display_version(&needed),
            display_version(theirs)
        ));
    }
    let installed_link = format!("{}/.local/bin/cagents", remote.home);
    let path_note = remote
        .off_path
        .then(|| unreachable_note(device, &installed_link));
    let Some(resolved) = &remote.resolved else {
        // Nothing to hand over from: an ordinary first install.
        return Ok(Plan {
            action: Action::Install {
                prefix: remote.default_prefix.clone(),
                upgrade: false,
            },
            note: (!leads_to(device, &installed_link))
                .then(|| unreachable_note(device, &installed_link)),
        });
    };
    let Some(installation) = remote.installation.as_ref().filter(|found| found.marked) else {
        return refuse(format!(
            "cagents on {name} ({resolved}) was not installed with install.sh, so this computer cannot update it. Update it on {name}, or replace it with an install.sh installation."
        ));
    };
    let prefix = &installation.prefix;
    let action = if installation.current.as_deref() == Some(version) {
        // Installed and served, but its daemon was left running the build
        // before: the installer defers an incompatible replacement.
        Action::Upgrade {
            cli: format!("{prefix}/current/bin/cagents"),
        }
    } else if installation.has_wanted
        && installation.wanted_staged_beside.is_some()
        && installation.wanted_staged_beside == installation.current
    {
        // Staged by an earlier run that stopped before the handover.
        Action::Upgrade {
            cli: format!("{prefix}/versions/{version}/bin/cagents"),
        }
    } else {
        Action::Install {
            prefix: prefix.clone(),
            upgrade: true,
        }
    };
    Ok(Plan {
        action,
        note: path_note,
    })
}

/// Whether the device's configured executable is the installer's link,
/// written out or as `~/.local/bin/cagents`.
fn leads_to(device: &Device, installed_link: &str) -> bool {
    device.executable == installed_link || device.executable == "~/.local/bin/cagents"
}

/// What to do when the device's configured executable does not lead to the
/// CLI this command installs or found.
fn unreachable_note(device: &Device, installed: &str) -> String {
    let name = &device.name;
    if device.executable == "cagents" {
        format!(
            "If connecting to {name} fails, cagents is not on the PATH SSH commands get there: set this device's executable to {installed}."
        )
    } else {
        format!(
            "This device's executable, {}, was not found on {name}. Set it to {installed}.",
            device.executable
        )
    }
}

/// The device's protocol when its daemon or its installed CLI, whichever is
/// newer, is newer than this computer's: a stopped daemon or a deferred
/// install must not hide a newer build.
fn newer(remote: &Remote, daemon: &Daemon) -> Option<u16> {
    [
        match daemon {
            Daemon::Speaks(actual) => Some(*actual),
            _ => None,
        },
        remote.protocol,
    ]
    .into_iter()
    .flatten()
    .max()
    .filter(|actual| *actual > PROTOCOL_VERSION)
}

fn daemon_state() -> Daemon {
    let socket = match super::existing_socket_path() {
        Ok(Some(socket)) => socket,
        Ok(None) => return Daemon::Unknown("no device is selected".into()),
        Err(error) => return Daemon::Unknown(error.to_string()),
    };
    match crate::control::ControlClient::connect(&socket) {
        Ok(_) => Daemon::Compatible,
        Err(crate::control::ControlError::UnsupportedVersion { actual }) => Daemon::Speaks(actual),
        Err(_) if super::existing_daemon_missing() => Daemon::Stopped,
        Err(error) => Daemon::Unknown(error.to_string()),
    }
}

/// Makes a private staging directory for one upload. An update interrupted
/// mid-upload cannot remove its own, so this removes those left for hours.
const PREPARE: &str = r#"umask 077
base=${XDG_CACHE_HOME:-$HOME/.cache}/cagents
mkdir -p "$base" || exit
find "$base" -maxdepth 1 -name 'device-update.*' -type d -mmin +360 -exec rm -rf -- {} + 2>/dev/null
staging=$(mktemp -d "$base/device-update.XXXXXXXX") || exit
printf 'staging=%s\n' "$staging"
"#;

/// SSH to the device for the length of one update. Its commands share one
/// authenticated connection; the daemon checks before and after go through
/// the device bridge's own.
struct Session {
    host: String,
    name: String,
    /// Holds the control socket; removed once the master has exited.
    _directory: tempfile::TempDir,
    control: Option<String>,
}

impl Session {
    fn open(device: &Device) -> io::Result<Self> {
        let runtime =
            crate::instance::runtime_directory()?.join(crate::instance::runtime_subdirectory());
        crate::secure_file::prepare_private_directory(&runtime)?;
        let directory = tempfile::Builder::new()
            .prefix("device-update-")
            .tempdir_in(&runtime)?;
        let control = control_path(directory.path(), 0);
        Ok(Self {
            host: device.host.clone(),
            name: device.name.clone(),
            _directory: directory,
            control,
        })
    }

    /// `remote`, a command line the device's login shell parses.
    fn command(&self, remote: &str) -> Command {
        let mut command = ssh(self.control.as_deref());
        command.args(["--", &self.host, remote]);
        command
    }

    /// Runs `script` in the device's POSIX shell; the login shell may not be one.
    fn script(&self, script: &str) -> io::Result<std::process::Output> {
        let mut child = self
            .command("sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(script.as_bytes())?;
        child.wait_with_output()
    }

    fn failure(&self, doing: &str, output: &std::process::Output) -> io::Error {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        io::Error::other(if detail.is_empty() {
            format!(
                "could not {doing} {} over SSH ({})",
                self.name, output.status
            )
        } else {
            format!("could not {doing} {} over SSH: {detail}", self.name)
        })
    }

    /// Writes stdin to `path` on the device, created private.
    fn upload(
        &self,
        path: &str,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut child = self
            .command(&format!(
                "sh -c 'umask 077 && cat > \"$1\"' sh {}",
                quote(path)
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let written = write(&mut stdin).and_then(|()| stdin.flush());
        drop(stdin);
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(self.failure("send files to", &output));
        }
        written
    }

    /// Sends this computer's build and stages it under `prefix` with its own
    /// installer. Returns the archive's size.
    fn upload_and_stage(
        &self,
        local: &Local,
        prefix: &str,
        progress: &mut dyn FnMut(Step),
    ) -> io::Result<u64> {
        let output = self.script(PREPARE)?;
        if !output.status.success() {
            return Err(self.failure("prepare", &output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let staging = stdout
            .lines()
            .find_map(|line| line.strip_prefix("staging="))
            .unwrap_or_default()
            .to_owned();
        if !staging.starts_with('/') {
            return Err(io::Error::other(format!(
                "{} returned an unusable staging directory",
                self.name
            )));
        }
        let result = self.stage_in(&staging, local, prefix, progress);
        // The installer copies what it needs; the upload is only its input.
        let _ = self.script(&format!("rm -rf -- {}\n", quote(&staging)));
        result
    }

    fn stage_in(
        &self,
        staging: &str,
        local: &Local,
        prefix: &str,
        progress: &mut dyn FnMut(Step),
    ) -> io::Result<u64> {
        let version = &local.version;
        let archive_name = format!("cagents-{version}-{TARGET}.tar.gz");
        let archive = format!("{staging}/{archive_name}");
        let total = tree_size(&local.root)?;
        let mut digest = None;
        let mut sent = 0;
        self.upload(&archive, |stdin| {
            let mut out = Hashing {
                inner: stdin,
                hasher: Sha256::new(),
                bytes: 0,
            };
            let mut reported = Reported::default();
            pack(
                &local.root,
                &format!("cagents-{version}"),
                &mut out,
                &mut |read| {
                    if reported.due(read, total) {
                        progress(Step::Sending { read, total });
                    }
                },
            )?;
            sent = out.bytes;
            digest = Some(out.hasher.finalize());
            Ok(())
        })?;
        let digest = digest.expect("written archive");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        self.upload(&format!("{staging}/SHA256SUMS"), |stdin| {
            writeln!(stdin, "{hex}  {archive_name}")
        })?;
        self.upload(&format!("{staging}/install.sh"), |stdin| {
            stdin.write_all(INSTALLER.as_bytes())
        })?;
        progress(Step::Installing);
        // The installer addresses someone at the device's desktop, so its
        // report is shown only when it fails.
        let output = self
            .command("sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().expect("piped stdin").write_all(
                    format!(
                        "CAGENTS_ARCHIVE={archive} CAGENTS_CHECKSUMS={sums} exec sh {installer} --version {version} --prefix {prefix} --stage-for-upgrade </dev/null\n",
                        archive = quote(&archive),
                        sums = quote(&format!("{staging}/SHA256SUMS")),
                        installer = quote(&format!("{staging}/install.sh")),
                        version = quote(version),
                        prefix = quote(prefix),
                    )
                    .as_bytes(),
                )?;
                child.wait_with_output()
            })?;
        if !output.status.success() {
            let report =
                String::from_utf8_lossy(&output.stdout) + String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "{name}'s installer did not stage cagents {version} ({status}). Nothing on {name} changed unless it says so:\n{report}",
                name = self.name,
                status = output.status,
                report = report.trim_end()
            )));
        }
        Ok(sent)
    }

    /// Hands the device's daemon over to the staged build through its own
    /// reviewed update. A person at a terminal answers its review there.
    fn upgrade(
        &self,
        device: &Device,
        cli: &str,
        release: Option<(&str, &str)>,
        options: Options,
    ) -> io::Result<()> {
        let interactive = !options.yes;
        let mut remote = format!("{} daemon upgrade", quote(cli));
        if let Some((version, digest)) = release {
            remote.push_str(&format!(
                " --version {} --require-release {} --require-protocol {PROTOCOL_VERSION}",
                quote(version),
                quote(digest)
            ));
        }
        if options.yes {
            remote.push_str(" --yes");
        }
        if options.interrupt {
            remote.push_str(" --interrupt");
        }
        let mut command = ssh(self.control.as_deref());
        if interactive {
            // The review prompts on a terminal; give it the person's.
            command.arg("-t");
        } else {
            command.stdin(Stdio::null());
        }
        // The review talks to the person; stdout is this command's result.
        command.stdout(io::stderr());
        command.args(["--", &self.host, &remote]);
        let status = command.stderr(Stdio::inherit()).status()?;
        if !status.success() {
            let staged = if release.is_some() {
                ""
            } else {
                " The build stays staged there, so retrying sends nothing."
            };
            return Err(io::Error::other(format!(
                "{name}'s daemon did not switch to cagents {version}. The device reported the reason above.{staged}",
                name = device.name,
                version = crate::version()
            )));
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(control) = &self.control
            && Path::new(control).exists()
        {
            let _ = ssh(Some(control))
                .args(["-O", "exit", "--", &self.host])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reports progress at whole-percent steps, at most every quarter second.
#[derive(Default)]
struct Reported {
    percent: Option<u64>,
    at: Option<Instant>,
}

impl Reported {
    fn due(&mut self, read: u64, total: u64) -> bool {
        let percent = read.saturating_mul(100) / total.max(1);
        let soon = self
            .at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(250));
        if self.percent == Some(percent) || (soon && percent < 100) {
            return false;
        }
        self.percent = Some(percent);
        self.at = Some(Instant::now());
        true
    }
}

/// The tree's top-level entries the archive carries: everything but the
/// installer's own hidden markers, which describe this computer's install.
fn members(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut members = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().as_encoded_bytes().starts_with(b".") {
            members.push(entry.path());
        }
    }
    members.sort();
    Ok(members)
}

fn tree_size(root: &Path) -> io::Result<u64> {
    fn walk(path: &Path) -> io::Result<u64> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            let mut total = 0;
            for entry in fs::read_dir(path)? {
                total += walk(&entry?.path())?;
            }
            Ok(total)
        } else {
            Ok(metadata.len())
        }
    }
    members(root)?.iter().map(|member| walk(member)).sum()
}

/// Writes `root` as the release archive layout, `top/bin/…`, gzip
/// compressed, reporting the bytes of files read so far. Links and special
/// files are refused: an installed tree has none, and the installer would
/// refuse them too.
fn pack(root: &Path, top: &str, out: impl Write, read: &mut dyn FnMut(u64)) -> io::Result<()> {
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        out,
        flate2::Compression::new(COMPRESSION),
    ));
    archive.mode(tar::HeaderMode::Deterministic);
    archive.follow_symlinks(false);
    append_directory(&mut archive, top, 0o755)?;
    let mut done = 0;
    for member in members(root)? {
        append(&mut archive, root, &member, top, &mut done, read)?;
    }
    archive.into_inner()?.finish()?.flush()
}

fn append<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    path: &Path,
    top: &str,
    done: &mut u64,
    read: &mut dyn FnMut(u64),
) -> io::Result<()> {
    let relative = path.strip_prefix(root).map_err(io::Error::other)?;
    let name = Path::new(top).join(relative);
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        append_directory(archive, &name.to_string_lossy(), metadata.mode())?;
        let mut entries = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            append(archive, root, &entry, top, done, read)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file; an installed build holds only files and directories",
            path.display()
        )));
    }
    let mut header = tar::Header::new_gnu();
    header.set_metadata_in_mode(&metadata, tar::HeaderMode::Deterministic);
    let file = fs::File::open(path)?;
    // The size is the open file's, so a file changing under us fails the
    // archive rather than corrupting it.
    header.set_size(file.metadata()?.len());
    let start = *done;
    let mut counting = Counting {
        inner: file,
        read: 0,
        report: &mut |bytes| read(start + bytes),
    };
    archive.append_data(&mut header, &name, &mut counting)?;
    *done = start + counting.read;
    Ok(())
}

/// install.sh accepts a directory entry only by a name ending in `/`.
fn append_directory<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    mode: u32,
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    // Deterministic mode, as for files: only the executable bits matter.
    header.set_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 });
    header.set_size(0);
    archive.append_data(&mut header, format!("{name}/"), io::empty())
}

struct Counting<'a, R> {
    inner: R,
    read: u64,
    report: &'a mut dyn FnMut(u64),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.read += count as u64;
        (self.report)(self.read);
        Ok(count)
    }
}

#[cfg(test)]
mod tests;
