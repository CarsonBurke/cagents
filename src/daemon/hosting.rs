//! Whether a hosted session may stop a development daemon.
//!
//! A hosted session must never stop the daemon that hosts it, or one hosting
//! a session it descends from: that ends its own terminal or chat and every
//! other session the daemon hosts. Nor may it stop the person's own daemons:
//! production, which a development CLI cannot address, and the default
//! development daemon in their login runtime. Any other development daemon has
//! a runtime directory of its own, such as one `scripts/run-development
//! --instance` starts, and a session that started it may also stop it.
//!
//! The hosts are found from the process tree rather than from anything the
//! session's environment claims. Every process a daemon hosts runs below one
//! of its own processes: the daemon itself, or a terminal holder or chat actor
//! it started from its own executable with its own environment. That stays
//! true after the daemon restarts and adopts its holders, and for daemons
//! built before this check existed.
//!
//! An instance's own processes leave the tree of the session that started it:
//! `niri-harness start` detaches its compositor. So `scripts/run-development
//! --instance` gives the instance [`HOSTED_BY_ENV`], the runtime directories of
//! every daemon above the starting session, and a daemon's environment carries
//! those too. A session the instance hosts then can't stop the daemons hosting
//! the session that started it.

use std::{
    ffi::OsStr,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use super::RuntimePaths;
use crate::instance;

const DAEMON_EXECUTABLE: &str = "cagentsd";
const MAX_ANCESTORS: usize = 1024;

/// Newline-separated runtime directories of the daemons hosting the session
/// that started a development instance; see the module documentation.
pub(crate) const HOSTED_BY_ENV: &str = "CAGENTS_DEVELOPMENT_HOSTED_BY";

pub(crate) const LIFECYCLE_REFUSAL: &str = "Daemon lifecycle commands require your own terminal; hosted sessions cannot start, stop or upgrade the daemon";

/// Why a hosted session may not stop the selected daemon.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A production CLI addresses the person's production daemon.
    Production,
    /// The daemon hosts this session or one it descends from.
    HostsCaller,
    /// The unnamed development daemon in the person's login runtime.
    PersonalDevelopment,
    /// The process tree could not show which daemons host this session.
    UnknownHost(String),
}

impl Refusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Production => LIFECYCLE_REFUSAL.into(),
            Self::HostsCaller => "This development daemon hosts this session or a session it was started from, so stopping it would end them".into(),
            Self::PersonalDevelopment => "The default development daemon in the login session belongs to the person using this computer; hosted sessions start and stop their own with scripts/run-development --instance NAME".into(),
            Self::UnknownHost(reason) => format!(
                "Could not tell which daemon hosts this session ({reason}), so this development daemon cannot be told apart from it"
            ),
        }
    }
}

/// Decides whether this hosted process may stop the daemon its environment
/// selects. `None` allows it.
pub(crate) fn stop_refusal() -> Option<Refusal> {
    if !instance::IS_DEVELOPMENT {
        return Some(Refusal::Production);
    }
    let target = match RuntimePaths::discover() {
        Ok(paths) => paths.directory,
        Err(error) => return Some(Refusal::UnknownHost(error.to_string())),
    };
    let login_runtime = PathBuf::from(format!("/run/user/{}", super::current_uid()));
    decide(
        &target,
        instance::development_instance().is_none(),
        &login_runtime,
        host_runtime_directories(&proc_root()),
    )
}

fn decide(
    target: &Path,
    unnamed: bool,
    login_runtime: &Path,
    hosts: Result<Vec<PathBuf>, String>,
) -> Option<Refusal> {
    if unnamed
        && target
            .parent()
            .is_some_and(|base| same_path(base, login_runtime))
    {
        return Some(Refusal::PersonalDevelopment);
    }
    match hosts {
        Ok(hosts) if hosts.iter().any(|host| same_path(host, target)) => Some(Refusal::HostsCaller),
        Ok(_) => None,
        Err(reason) => Some(Refusal::UnknownHost(reason)),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (fs::canonicalize(left), fs::canonicalize(right)),
            (Ok(left), Ok(right)) if left == right
        )
}

/// The runtime directories of every daemon above this process: each directory
/// the daemon's environment could select, production or development, since its
/// build is not visible from outside. `self` names this process in the
/// namespace of the `/proc` being read, which its own ID might not.
fn host_runtime_directories(proc_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut directories = Vec::new();
    let mut process: u32 = fs::read_link(proc_root.join("self"))
        .ok()
        .and_then(|process| process.to_str()?.parse().ok())
        .ok_or_else(|| format!("cannot find this process in {}", proc_root.display()))?;
    for _ in 0..MAX_ANCESTORS {
        let directory = proc_root.join(process.to_string());
        if runs_daemon_executable(&directory) {
            let environment = fs::read(directory.join("environ")).map_err(|error| {
                format!("cannot read the environment of process {process}: {error}")
            })?;
            directories.extend(daemon_runtime_directories(&environment).ok_or_else(|| {
                format!(
                    "process {process} runs {DAEMON_EXECUTABLE} without an absolute XDG_RUNTIME_DIR"
                )
            })?);
        }
        let parent = parent_of(&directory)
            .map_err(|error| format!("cannot read the parent of process {process}: {error}"))?;
        if parent <= 1 {
            return if directories.is_empty() {
                Err(format!(
                    "no {DAEMON_EXECUTABLE} process is among this command's parents"
                ))
            } else {
                Ok(directories)
            };
        }
        process = parent;
    }
    Err(format!("over {MAX_ANCESTORS} parent processes"))
}

/// Holders and actors keep the daemon's executable under another process name.
/// A process this user can't inspect is not one of their daemons.
fn runs_daemon_executable(process: &Path) -> bool {
    fs::read_link(process.join("exe")).is_ok_and(|executable| {
        executable.file_name().is_some_and(|name| {
            let name = name.as_bytes();
            name.strip_suffix(b" (deleted)").unwrap_or(name) == DAEMON_EXECUTABLE.as_bytes()
        })
    })
}

fn parent_of(process: &Path) -> io::Result<u32> {
    let stat = fs::read(process.join("stat"))?;
    // The command name in parentheses may itself contain spaces or parentheses.
    let fields = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .map(|end| &stat[end + 1..])
        .ok_or_else(|| io::Error::other("malformed stat"))?;
    std::str::from_utf8(fields)
        .ok()
        .and_then(|fields| fields.split_ascii_whitespace().nth(1))
        .and_then(|parent| parent.parse().ok())
        .ok_or_else(|| io::Error::other("malformed stat"))
}

fn daemon_runtime_directories(environment: &[u8]) -> Option<Vec<PathBuf>> {
    let variable = |name: &str| {
        environment.split(|byte| *byte == 0).find_map(|entry| {
            entry
                .strip_prefix(name.as_bytes())
                .and_then(|rest| rest.strip_prefix(b"="))
        })
    };
    let base = Path::new(OsStr::from_bytes(variable("XDG_RUNTIME_DIR")?));
    if !base.is_absolute() {
        return None;
    }
    let mut directories = vec![base.join("cagents"), base.join("cagents-development")];
    if let Some(name) = variable(instance::DEVELOPMENT_INSTANCE_ENV).filter(|name| !name.is_empty())
    {
        let mut namespace = b"cagents-development-".to_vec();
        namespace.extend_from_slice(name);
        directories.push(base.join(OsStr::from_bytes(&namespace)));
    }
    if let Some(hosts) = variable(HOSTED_BY_ENV) {
        directories.extend(
            hosts
                .split(|byte| *byte == b'\n')
                .map(|host| Path::new(OsStr::from_bytes(host)))
                .filter(|host| host.is_absolute())
                .map(Path::to_path_buf),
        );
    }
    Some(directories)
}

#[cfg(not(test))]
fn proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

/// Process-tree fixtures for this process. A test that stops a daemon must
/// never let the real tree decide, which differs between a hosted terminal
/// and the build host.
#[cfg(test)]
pub(crate) static TEST_PROC_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn proc_root() -> PathBuf {
    TEST_PROC_ROOT
        .lock()
        .unwrap()
        .clone()
        .expect("a test reaching the hosted stop check must choose its process tree")
}

/// A fake `/proc` in which `self`, process 40, runs below a host whose
/// environment selects `runtime` and, optionally, a development instance.
#[cfg(test)]
pub(crate) fn fake_process_tree(root: &Path, runtime: &Path, instance: Option<&str>) {
    let pid = 40;
    let write = |pid: u32, parent: u32, executable: &str, environment: &[u8]| {
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("stat"),
            format!("{pid} (odd ) name) S {parent} 0 0\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink(executable, directory.join("exe")).unwrap();
        fs::write(directory.join("environ"), environment).unwrap();
    };
    let mut environment = b"HOME=/fixture\0XDG_RUNTIME_DIR=".to_vec();
    environment.extend_from_slice(runtime.as_os_str().as_bytes());
    if let Some(instance) = instance {
        environment
            .extend_from_slice(format!("\0CAGENTS_DEVELOPMENT_INSTANCE={instance}").as_bytes());
    }
    environment.push(0);
    write(pid, pid + 1, "/usr/bin/sh", b"");
    std::os::unix::fs::symlink(pid.to_string(), root.join("self")).unwrap();
    write(
        pid + 1,
        1,
        "/fixture/install/bin/cagentsd (deleted)",
        &environment,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(runtime: &str, instance: Option<&str>) -> Result<Vec<PathBuf>, String> {
        let root = tempfile::tempdir().unwrap();
        fake_process_tree(root.path(), Path::new(runtime), instance);
        host_runtime_directories(root.path())
    }

    #[test]
    fn a_hosts_runtime_names_every_daemon_its_environment_selects() {
        assert_eq!(
            hosts("/run/user/7", Some("b2242")).unwrap(),
            [
                "/run/user/7/cagents",
                "/run/user/7/cagents-development",
                "/run/user/7/cagents-development-b2242",
            ]
            .map(PathBuf::from)
        );
        assert_eq!(hosts("/run/user/7", None).unwrap().len(), 2);
    }

    /// Puts a daemon process `pid`, with `environment`, between `self` (40)
    /// and its parent.
    fn insert_daemon(root: &Path, pid: u32, environment: &[u8]) {
        let parent = parent_of(&root.join("40")).unwrap();
        let directory = root.join(pid.to_string());
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("stat"),
            format!("{pid} (cagents-native) S {parent} 0\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink("/w/target/dev-runtime/cagentsd", directory.join("exe"))
            .unwrap();
        fs::write(directory.join("environ"), environment).unwrap();
        fs::write(root.join("40/stat"), format!("40 (sh) S {pid} 0\n")).unwrap();
    }

    #[test]
    fn every_daemon_above_the_caller_counts_not_only_the_nearest() {
        let root = tempfile::tempdir().unwrap();
        // Hosted by an instance whose UI a production-hosted session started.
        fake_process_tree(root.path(), Path::new("/run/user/7"), None);
        insert_daemon(
            root.path(),
            30,
            b"XDG_RUNTIME_DIR=/run/user/7/nh-x\0CAGENTS_DEVELOPMENT_INSTANCE=a\0",
        );
        let directories = host_runtime_directories(root.path()).unwrap();
        assert!(directories.contains(&PathBuf::from("/run/user/7/nh-x/cagents-development-a")));
        assert!(directories.contains(&PathBuf::from("/run/user/7/cagents")));
    }

    #[test]
    fn an_instance_keeps_the_hosts_of_the_session_that_started_it() {
        let root = tempfile::tempdir().unwrap();
        // The harness detached instance b from the session in instance a
        // that started it, so a is only in b's record.
        fake_process_tree(root.path(), Path::new("/run/user/7/nh-b"), Some("b"));
        fs::write(
            root.path().join("41/environ"),
            b"XDG_RUNTIME_DIR=/run/user/7/nh-b\0CAGENTS_DEVELOPMENT_INSTANCE=b\0\
              CAGENTS_DEVELOPMENT_HOSTED_BY=/run/user/7/nh-a/cagents-development-a\n\
              relative\n/run/user/7/cagents\0",
        )
        .unwrap();
        let directories = host_runtime_directories(root.path()).unwrap();
        for host in [
            "/run/user/7/nh-b/cagents-development-b",
            "/run/user/7/nh-a/cagents-development-a",
            "/run/user/7/cagents",
        ] {
            assert!(directories.contains(&PathBuf::from(host)), "{host}");
        }
        assert!(!directories.contains(&PathBuf::from("relative")));
    }

    #[test]
    fn a_tree_without_a_daemon_or_an_unreadable_one_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("5");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("stat"), "5 (sh) S 1 0\n").unwrap();
        std::os::unix::fs::symlink("5", root.path().join("self")).unwrap();
        assert!(host_runtime_directories(root.path()).is_err());

        let root = tempfile::tempdir().unwrap();
        assert!(host_runtime_directories(root.path()).is_err());

        let root = tempfile::tempdir().unwrap();
        fake_process_tree(root.path(), Path::new("/run/user/7"), None);
        fs::remove_file(root.path().join("41/environ")).unwrap();
        assert!(host_runtime_directories(root.path()).is_err());

        assert!(hosts("relative", None).is_err());
    }

    #[test]
    fn only_isolated_daemons_that_host_nothing_above_the_caller_may_stop() {
        let login = Path::new("/run/user/7");
        let production_host = Ok(vec![PathBuf::from("/run/user/7/cagents")]);
        // An instance the runner started in its own harness runtime.
        assert_eq!(
            decide(
                Path::new("/run/user/7/nh-a/cagents-development-b2242"),
                false,
                login,
                production_host.clone(),
            ),
            None
        );
        // A private fixture runtime with the unnamed namespace.
        assert_eq!(
            decide(
                Path::new("/var/tmp/fx/rt/cagents-development"),
                true,
                login,
                production_host.clone(),
            ),
            None
        );
        // The person's own development daemon.
        assert_eq!(
            decide(
                Path::new("/run/user/7/cagents-development"),
                true,
                login,
                production_host,
            ),
            Some(Refusal::PersonalDevelopment)
        );
        // The instance hosting this session.
        let target = Path::new("/run/user/7/nh-a/cagents-development-b2242");
        assert_eq!(
            decide(target, false, login, Ok(vec![target.to_owned()])),
            Some(Refusal::HostsCaller)
        );
        assert!(matches!(
            decide(target, false, login, Err("no tree".into())),
            Some(Refusal::UnknownHost(_))
        ));
    }
}
