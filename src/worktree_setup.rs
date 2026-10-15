//! Trusted, shared preparation for worktree setup terminals.
//!
//! Creation remains successful if preparation or the terminal fails. Callers
//! retain the created checkout, display the error, and own terminal cancellation.
//! This module never executes repository-supplied shell commands itself.

use crate::{
    fs_safety,
    process::{CancellationToken, ProcessExecutor, SystemProcessExecutor},
    repository::{GitFileLock, RepositoryService},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    path::{Path, PathBuf},
};

const CONFIG_PATH: &str = ".cagents/worktree.toml";
const CONFIG_LIMIT: u64 = 64 * 1024;
const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const SCAN_LIMIT: usize = 100_000;
const COPY_LIMIT: usize = 10_000;
const TRUST_KEY: &str = "cagents.worktreeSetupTrust";

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("Worktree setup: {0}")]
    Filesystem(#[from] fs_safety::ResolveError),
    #[error("Worktree setup: {0}")]
    Invalid(String),
    #[error("Trust this project's setup commands before running setup")]
    TrustRequired,
    #[error("Worktree setup configuration changed; review it again")]
    ConfigChanged,
    #[error("Worktree setup cancelled; the created worktree was kept")]
    Cancelled,
}

fn error(error: impl std::fmt::Display) -> SetupError {
    SetupError::Invalid(error.to_string())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), SetupError> {
    if cancel.is_cancelled() {
        Err(SetupError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
enum Commands {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    copy: Vec<String>,
    setup: Option<Commands>,
}

pub use crate::repository::WorktreeSetupPlan as SetupPlan;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCopy {
    pub path: PathBuf,
    pub reason: String,
}

/// The caller opens `command` in an interactive terminal card in the new
/// checkout, and projects that terminal's running/success/failure state on its row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPreparation {
    pub skipped: bool,
    pub copied: Vec<PathBuf>,
    pub skipped_paths: Vec<SkippedCopy>,
    pub missing_patterns: Vec<String>,
    pub command: Option<String>,
}

/// Load a checked-in configuration from the primary checkout, not the new branch.
/// A missing config is normal. Symbolic links and oversized configs are refused.
pub fn load(primary: &Path) -> Result<Option<SetupPlan>, SetupError> {
    let root = fs_safety::Root::open(primary).map_err(error)?;
    let mut file = match fs_safety::open_beneath(&root, Path::new(CONFIG_PATH)) {
        Ok(file) => file,
        Err(err) if err.is_not_found() => return Ok(None),
        Err(err) => return Err(error(err)),
    };
    if !file.metadata().map_err(error)?.is_file() {
        return Err(error("setup config must be a regular file"));
    }
    let bytes = fs_safety::read_bounded(&mut file, CONFIG_LIMIT).map_err(error)?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(error("setup config exceeds 64 KiB"));
    }
    let config: Config =
        toml::from_str(std::str::from_utf8(&bytes).map_err(error)?).map_err(error)?;
    if config.copy.len() > 128 {
        return Err(error("at most 128 copy patterns are allowed"));
    }
    for pattern in &config.copy {
        pattern_components(pattern)?;
    }
    let commands = match config.setup {
        None => vec![],
        Some(Commands::One(command)) => vec![command],
        Some(Commands::Many(commands)) => commands,
    };
    if commands.len() > 32
        || commands
            .iter()
            .any(|s| s.trim().is_empty() || s.contains('\0'))
    {
        return Err(error(
            "setup needs at most 32 nonempty commands without NUL bytes",
        ));
    }
    Ok(Some(SetupPlan {
        copy: config.copy,
        commands,
        fingerprint: format!("{:x}", Sha256::digest(&bytes)),
    }))
}

fn current(primary: &Path, plan: &SetupPlan) -> Result<(), SetupError> {
    if load(primary)?.as_ref() != Some(plan) {
        return Err(SetupError::ConfigChanged);
    }
    Ok(())
}

fn git(
    primary: &Path,
    args: impl IntoIterator<Item = OsString>,
    cancel: &CancellationToken,
) -> Result<crate::process::ProcessOutput, SetupError> {
    check_cancel(cancel)?;
    // Share the repository service's invocation boundary: inherited Git
    // redirection variables must never select another clone or index.
    let spec = RepositoryService::system("git")
        .git_base(primary, true)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args);
    SystemProcessExecutor.execute(&spec, cancel).map_err(error)
}

pub fn is_trusted(
    primary: &Path,
    plan: &SetupPlan,
    cancel: &CancellationToken,
) -> Result<bool, SetupError> {
    if !plan.requires_trust() {
        return Ok(true);
    }
    let output = git(
        primary,
        ["config", "--local", "--get", TRUST_KEY].map(OsString::from),
        cancel,
    )?;
    match output.exit_code {
        Some(0) => Ok(output.stdout == format!("{}\n", plan.fingerprint).as_bytes()),
        Some(1) => Ok(false),
        _ => Err(error("could not read this project's setup trust")),
    }
}

/// Call only after explicit user approval of this exact plan for this project.
/// A repository file cannot grant trust to itself, and changing config revokes it.
pub fn trust(
    primary: &Path,
    plan: &SetupPlan,
    cancel: &CancellationToken,
) -> Result<(), SetupError> {
    current(primary, plan)?;
    let output = git(
        primary,
        [
            "config",
            "--local",
            "--replace-all",
            TRUST_KEY,
            &plan.fingerprint,
        ]
        .map(OsString::from),
        cancel,
    )?;
    if !output.success {
        return Err(error("could not save this project's setup trust"));
    }
    Ok(())
}

fn pattern_components(pattern: &str) -> Result<Vec<&str>, SetupError> {
    if pattern.is_empty() || pattern.contains(['\0', '\\']) || Path::new(pattern).is_absolute() {
        return Err(error("copy patterns must be nonempty relative glob paths"));
    }
    let parts: Vec<_> = pattern.split('/').collect();
    if parts.len() > 32
        || parts
            .iter()
            .any(|s| s.is_empty() || matches!(*s, "." | ".." | ".git"))
    {
        return Err(error(
            "copy patterns cannot escape the checkout or select .git",
        ));
    }
    glob::Pattern::new(pattern).map_err(error)?;
    Ok(parts)
}

fn matches(
    root: &fs_safety::Root,
    parts: &[&str],
    path: &Path,
    scanned: &mut usize,
    found: &mut BTreeSet<PathBuf>,
    cancel: &CancellationToken,
) -> Result<(), SetupError> {
    check_cancel(cancel)?;
    if parts.is_empty() {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        match fs_safety::metadata_beneath(root, path) {
            Ok(metadata) if metadata.is_file() => {
                found.insert(path.into());
            }
            Ok(_) => {}
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(error(err)),
        }
        if found.len() > COPY_LIMIT {
            return Err(error("copy matches exceed 10000 files"));
        }
        return Ok(());
    }
    if parts[0] == "**" {
        matches(root, &parts[1..], path, scanned, found, cancel)?;
    }
    // Exact names need no directory enumeration, including in a huge or
    // searchable-but-not-readable primary checkout.
    let entries = if !parts[0].contains(['*', '?', '[']) {
        vec![OsString::from(parts[0])]
    } else {
        let entries = match fs_safety::directory_entries_beneath(
            root,
            path,
            SCAN_LIMIT.saturating_sub(*scanned),
        ) {
            Ok(entries) => entries,
            Err(err) if err.is_not_found() => return Ok(()),
            Err(err) => return Err(error(err)),
        };
        *scanned += entries.len();
        if *scanned > SCAN_LIMIT {
            return Err(error("copy scan exceeds 100000 directory entries"));
        }
        entries
    };
    let component = glob::Pattern::new(parts[0]).map_err(error)?;
    for name in entries {
        check_cancel(cancel)?;
        if name == ".git" || (!component.matches_path(Path::new(&name)) && parts[0] != "**") {
            continue;
        }
        let child = path.join(name);
        let metadata = match fs_safety::metadata_beneath(root, &child) {
            Ok(metadata) => metadata,
            // Symlinks and unreadable entries cannot be copied or traversed.
            Err(_) => continue,
        };
        // A primary checkout may physically contain managed worktrees.
        // Never recurse into another Git checkout, including the destination
        // itself, when a broad glob such as **/.env is configured.
        if metadata.is_dir() {
            match fs_safety::metadata_beneath(root, &child.join(".git")) {
                Ok(_) => continue,
                Err(err) if err.is_not_found() => {}
                Err(err) => return Err(error(err)),
            }
        }
        if parts[0] == "**" {
            if metadata.is_dir() {
                matches(root, parts, &child, scanned, found, cancel)?;
            } else if parts.len() == 1 && metadata.is_file() {
                found.insert(child);
            }
        } else if parts.len() == 1 {
            if metadata.is_file() {
                found.insert(child);
            }
        } else if metadata.is_dir() {
            matches(root, &parts[1..], &child, scanned, found, cancel)?;
        }
        if found.len() > COPY_LIMIT {
            return Err(error("copy matches exceed 10000 files"));
        }
    }
    Ok(())
}

/// Copy approved files and return a shell recipe; never run it implicitly.
/// Skip is checked before config, Git, trust, or filesystem effects. Commands
/// require trust before copies begin. Copy-only configurations need no trust.
pub fn prepare(
    primary: &Path,
    destination: &Path,
    plan: &SetupPlan,
    skip: bool,
    cancel: &CancellationToken,
) -> Result<SetupPreparation, SetupError> {
    if skip {
        return Ok(SetupPreparation {
            skipped: true,
            ..Default::default()
        });
    }
    check_cancel(cancel)?;
    let _protection = crate::worktree_cleanup::protect(destination).map_err(error)?;
    current(primary, plan)?;
    if !is_trusted(primary, plan, cancel)? {
        return Err(SetupError::TrustRequired);
    }
    let service = RepositoryService::system("git");
    let common = service.common_dir(primary, cancel).map_err(error)?;
    if service.common_dir(destination, cancel).map_err(error)? != common
        || fs::canonicalize(primary).map_err(error)?
            == fs::canonicalize(destination).map_err(error)?
    {
        return Err(error(
            "setup source and destination must be different checkouts of the same repository",
        ));
    }
    let source = fs_safety::Root::open(primary).map_err(error)?;
    let target = fs_safety::Root::open(destination).map_err(error)?;
    let mut result = SetupPreparation::default();
    let mut found = BTreeSet::new();
    let mut scanned = 0;
    for pattern in &plan.copy {
        let mut matched = BTreeSet::new();
        matches(
            &source,
            &pattern_components(pattern)?,
            Path::new(""),
            &mut scanned,
            &mut matched,
            cancel,
        )?;
        if matched.is_empty() {
            result.missing_patterns.push(pattern.clone());
        }
        found.extend(matched);
        if found.len() > COPY_LIMIT {
            return Err(error("copy matches exceed 10000 files"));
        }
    }
    let admin = service.git_dir(destination, cancel).map_err(error)?;
    let index_lock = GitFileLock::acquire(&admin.join("index")).map_err(error)?;
    let mut tracked = HashSet::new();
    // ls-files has no stdin pathspec support. Bound argv and output to the
    // selected candidates rather than reading an unrelated huge index.
    let mut pending = found.iter().peekable();
    while pending.peek().is_some() {
        check_cancel(cancel)?;
        let mut args: Vec<OsString> = ["ls-files", "--cached", "-z", "--"]
            .map(OsString::from)
            .into();
        let mut size = 0;
        while let Some(path) = pending.peek() {
            let length = path.as_os_str().as_encoded_bytes().len() + 1;
            if size + length > 32 * 1024 && size > 0 {
                break;
            }
            args.push(path.as_os_str().to_owned());
            size += length;
            pending.next();
        }
        let output = git(destination, args, cancel)?;
        if !output.success {
            return Err(error("could not read destination tracked files"));
        }
        tracked.extend(
            output
                .stdout
                .split(|b| *b == 0)
                .filter(|b| !b.is_empty())
                .map(|b| PathBuf::from(OsString::from_vec(b.to_vec()))),
        );
    }
    for path in found {
        check_cancel(cancel)?;
        index_lock.check().map_err(error)?;
        if tracked.contains(&path) {
            result.skipped_paths.push(SkippedCopy {
                path,
                reason: "tracked destination is never overwritten".into(),
            });
            continue;
        }
        let mut input = match fs_safety::open_beneath(&source, &path) {
            Ok(file) => file,
            Err(err) if err.is_not_found() => {
                result.skipped_paths.push(SkippedCopy {
                    path,
                    reason: "source disappeared".into(),
                });
                continue;
            }
            Err(err) => return Err(error(err)),
        };
        let metadata = input.metadata().map_err(error)?;
        if !metadata.is_file() || metadata.len() > FILE_LIMIT {
            return Err(error(format!(
                "{} is not a regular file up to 64 MiB",
                path.display()
            )));
        }
        let parent = fs_safety::create_directories_beneath(
            &target,
            path.parent().unwrap_or(Path::new("")),
            0o700,
        )
        .map_err(error)?;
        let (mut output, temporary) =
            fs_safety::create_temporary_in(&parent, 0o600).map_err(error)?;
        let copied = (|| {
            let mut total = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                check_cancel(cancel)?;
                let read = input.read(&mut buffer).map_err(error)?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                if total > FILE_LIMIT {
                    return Err(error("source grew beyond the copy limit"));
                }
                output.write_all(&buffer[..read]).map_err(error)?;
            }
            output
                .set_permissions(fs::Permissions::from_mode(
                    metadata.permissions().mode() & 0o777,
                ))
                .map_err(error)?;
            // Numeric group mode bits can describe the ACL mask rather than
            // actual group access. Preserve the source ACL and remove any
            // default ACL inherited by a new copy when the source has none.
            fs_safety::copy_access_acl(&input, &output).map_err(error)?;
            output.sync_all().map_err(error)?;
            check_cancel(cancel)?;
            index_lock.check().map_err(error)?;
            fs_safety::rename_new_within(
                &parent,
                &temporary,
                path.file_name().expect("matched file has a name"),
            )
            .map_err(SetupError::Filesystem)
        })();
        match copied {
            Ok(()) => {
                fs_safety::sync_directory(&parent).map_err(error)?;
                result.copied.push(path);
            }
            Err(err) => {
                let cleanup = fs_safety::unlink_within(&parent, &temporary);
                // Existing destination is always kept, including dangling symlinks.
                if matches!(&err, SetupError::Filesystem(cause) if cause.is_already_exists())
                    && cleanup.is_ok()
                {
                    result.skipped_paths.push(SkippedCopy {
                        path,
                        reason: "destination already exists".into(),
                    });
                } else {
                    cleanup.map_err(error)?;
                    return Err(err);
                }
            }
        }
    }
    check_cancel(cancel)?;
    current(primary, plan)?;
    if !is_trusted(primary, plan, cancel)? {
        return Err(SetupError::TrustRequired);
    }
    result.command = (!plan.commands.is_empty()).then(|| {
        plan.commands
            .iter()
            .map(|command| format!("/bin/sh -c '{}'", command.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" && ")
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, process::Command};

    struct Fixture {
        _directory: tempfile::TempDir,
        primary: PathBuf,
        destination: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_nested_destination(false)
        }
        fn with_nested_destination(nested: bool) -> Self {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let primary = directory.path().join("primary");
            let destination = if nested {
                primary.join(".worktrees/new")
            } else {
                directory.path().join("new")
            };
            fs::create_dir(&primary).unwrap();
            let fixture = Self {
                _directory: directory,
                primary,
                destination,
            };
            fixture.git(&["init", "-b", "main"]);
            fs::write(fixture.primary.join("tracked"), "committed").unwrap();
            fixture.git(&["add", "tracked"]);
            fixture.git(&[
                "-c",
                "user.name=Contract",
                "-c",
                "user.email=contract@example.invalid",
                "commit",
                "-m",
                "initial",
            ]);
            fixture.git(&[
                "worktree",
                "add",
                "--detach",
                fixture.destination.to_str().unwrap(),
            ]);
            fixture
        }
        fn git(&self, args: &[&str]) {
            let output = Command::new("git")
                .current_dir(&self.primary)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fn config(&self, text: &str) -> SetupPlan {
            fs::create_dir_all(self.primary.join(".cagents")).unwrap();
            fs::write(self.primary.join(CONFIG_PATH), text).unwrap();
            load(&self.primary).unwrap().unwrap()
        }
        fn prepare(&self, plan: &SetupPlan) -> Result<SetupPreparation, SetupError> {
            prepare(
                &self.primary,
                &self.destination,
                plan,
                false,
                &CancellationToken::new(),
            )
        }
    }

    #[test]
    fn parses_both_command_forms_and_rejects_unknown_or_escaping_config() {
        let fixture = Fixture::new();
        assert!(load(&fixture.primary).unwrap().is_none());
        assert_eq!(fixture.config("setup = 'pnpm i'\n").commands, ["pnpm i"]);
        assert_eq!(
            fixture.config("setup = ['one', 'two']\n").commands,
            ["one", "two"]
        );
        for text in [
            "copy = ['../secret']",
            "copy = ['.git/config']",
            "copy = ['/etc/passwd']",
            "copy = ['a//b']",
            "copy = ['[']",
            "setpu = 'oops'",
            "setup = ''",
        ] {
            fs::write(fixture.primary.join(CONFIG_PATH), text).unwrap();
            assert!(load(&fixture.primary).is_err(), "{text}");
        }
    }

    #[test]
    fn copy_only_preserves_modes_skips_missing_sources_and_never_overwrites_tracked_files() {
        let fixture = Fixture::new();
        let plan = fixture.config("copy = ['.env*', 'tracked', 'missing/local.toml']");
        fs::write(fixture.primary.join(".env"), "secret").unwrap();
        fs::set_permissions(
            fixture.primary.join(".env"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        fs::write(fixture.primary.join("tracked"), "local change").unwrap();
        // Even a tracked file absent in the destination must not be recreated.
        fs::remove_file(fixture.destination.join("tracked")).unwrap();
        let result = fixture.prepare(&plan).unwrap();
        assert_eq!(result.copied, [PathBuf::from(".env")]);
        assert_eq!(
            fs::read(fixture.destination.join(".env")).unwrap(),
            b"secret"
        );
        assert_eq!(
            fs::metadata(fixture.destination.join(".env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert!(!fixture.destination.join("tracked").exists());
        assert_eq!(result.skipped_paths[0].path, PathBuf::from("tracked"));
        assert_eq!(result.missing_patterns, ["missing/local.toml"]);
        assert!(result.command.is_none());
        let admin = RepositoryService::system("git")
            .git_dir(&fixture.destination, &CancellationToken::new())
            .unwrap();
        assert!(!admin.join("index.lock").exists());
    }

    #[test]
    fn literal_copy_works_without_enumerating_primary_directory() {
        let fixture = Fixture::new();
        fs::write(fixture.primary.join(".env"), "secret").unwrap();
        let plan = fixture.config("copy = ['.env']");
        fs::set_permissions(&fixture.primary, fs::Permissions::from_mode(0o111)).unwrap();
        let result = fixture.prepare(&plan);
        fs::set_permissions(&fixture.primary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result.unwrap().copied, [PathBuf::from(".env")]);
        assert_eq!(fs::read(fixture.destination.join(".env")).unwrap(), b"secret");
    }

    #[test]
    fn copy_preserves_restrictive_access_acl_and_removes_inherited_acl() {
        use std::os::fd::AsRawFd;
        fn write_acl(file: &fs::File, name: &std::ffi::CStr, value: &[u8]) -> bool {
            // SAFETY: file and both borrowed buffers outlive this call.
            let result = unsafe { libc::fsetxattr(file.as_raw_fd(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
            if result == 0 { return true; }
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)) {
                eprintln!("SETUP_ACL_FIXTURE_UNSUPPORTED: {error}");
                return false;
            }
            panic!("could not set fixture ACL: {error}");
        }
        fn read_acl(file: &fs::File) -> Option<Vec<u8>> {
            let name = c"system.posix_acl_access";
            // SAFETY: file/name remain live; null with zero size queries length.
            let size = unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
            if size < 0 {
                let error = std::io::Error::last_os_error();
                assert_eq!(error.raw_os_error(), Some(libc::ENODATA), "{error}");
                return None;
            }
            let mut value = vec![0u8; size as usize];
            // SAFETY: value is writable for its entire passed length.
            let read = unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
            assert_eq!(read, size);
            Some(value)
        }
        let fixture = Fixture::new();
        fs::write(fixture.primary.join(".env.restricted"), "restricted").unwrap();
        fs::write(fixture.primary.join(".env.plain"), "plain").unwrap();
        let source = fs::File::open(fixture.primary.join(".env.restricted")).unwrap();
        // Linux POSIX ACL v2: group mode0640 is the readable mask; the owning
        // group itself has no access. A mode-only copy would broaden access.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01_u16, 6_u16, u32::MAX), (0x02, 4, 12_345),
            (0x04, 0, u32::MAX), (0x10, 4, u32::MAX), (0x20, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        if !write_acl(&source, c"system.posix_acl_access", &acl) { return; }
        assert_eq!(source.metadata().unwrap().permissions().mode() & 0o777, 0o640);
        let parent = fs::File::open(&fixture.destination).unwrap();
        assert!(write_acl(&parent, c"system.posix_acl_default", &acl));
        fs::set_permissions(fixture.primary.join(".env.plain"), fs::Permissions::from_mode(0o640)).unwrap();
        let plan = fixture.config("copy = ['.env*']");
        assert_eq!(fixture.prepare(&plan).unwrap().copied.len(), 2);
        let restricted = fs::File::open(fixture.destination.join(".env.restricted")).unwrap();
        let plain = fs::File::open(fixture.destination.join(".env.plain")).unwrap();
        assert_eq!(read_acl(&restricted), Some(acl));
        assert_eq!(read_acl(&plain), None);
        assert_eq!(plain.metadata().unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(fs::read(fixture.destination.join(".env.restricted")).unwrap(), b"restricted");
        assert_eq!(fs::read(fixture.destination.join(".env.plain")).unwrap(), b"plain");
        println!("SETUP_ACL_COPY_VERIFIED");
    }

    #[test]
    fn recursive_globs_are_bounded_to_real_directories_and_copy_nested_files() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.primary.join("config/nested")).unwrap();
        fs::write(fixture.primary.join("config/root.pem"), "root").unwrap();
        fs::write(fixture.primary.join("config/nested/cert.pem"), "cert").unwrap();
        let outside = fixture._directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.pem"), "outside").unwrap();
        symlink(&outside, fixture.primary.join("config/link")).unwrap();
        let plan = fixture.config("copy = ['config/**/*.pem']");
        let result = fixture.prepare(&plan).unwrap();
        assert_eq!(result.copied.len(), 2);
        assert_eq!(
            fs::read(fixture.destination.join("config/nested/cert.pem")).unwrap(),
            b"cert"
        );
        assert!(!fixture.destination.join("config/link").exists());
    }

    #[test]
    fn broad_globs_do_not_copy_other_managed_worktrees_or_the_destination() {
        let fixture = Fixture::with_nested_destination(true);
        let other = fixture.primary.join(".worktrees/other");
        fixture.git(&["worktree", "add", "--detach", other.to_str().unwrap()]);
        fs::write(fixture.primary.join(".env"), "primary secret").unwrap();
        fs::write(other.join(".env"), "other agent secret").unwrap();
        let plan = fixture.config("copy = ['**/.env']");
        let result = fixture.prepare(&plan).unwrap();
        assert_eq!(result.copied, [PathBuf::from(".env")]);
        assert_eq!(
            fs::read(fixture.destination.join(".env")).unwrap(),
            b"primary secret"
        );
        assert!(!fixture.destination.join(".worktrees").exists());
        assert_eq!(fs::read(other.join(".env")).unwrap(), b"other agent secret");
        // A second preparation must not feed the destination's new .env back
        // into a nested copy of itself.
        assert!(fixture.prepare(&plan).unwrap().copied.is_empty());
        assert!(!fixture.destination.join(".worktrees").exists());
    }

    #[test]
    fn refuses_symlinked_destination_parent_and_keeps_outside_content() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.primary.join("config")).unwrap();
        fs::write(fixture.primary.join("config/local"), "source").unwrap();
        let outside = fixture._directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, fixture.destination.join("config")).unwrap();
        let plan = fixture.config("copy = ['config/local']");
        assert!(fixture.prepare(&plan).is_err());
        assert!(!outside.join("local").exists());
        assert!(fixture.destination.exists());
    }

    #[test]
    fn keeps_existing_untracked_and_dangling_symlink_destinations() {
        let fixture = Fixture::new();
        fs::write(fixture.primary.join(".env"), "source").unwrap();
        fs::write(fixture.primary.join(".env.local"), "source local").unwrap();
        fs::write(fixture.destination.join(".env"), "existing").unwrap();
        symlink("missing", fixture.destination.join(".env.local")).unwrap();
        let plan = fixture.config("copy = ['.env*']");
        let result = fixture.prepare(&plan).unwrap();
        assert!(result.copied.is_empty());
        assert_eq!(result.skipped_paths.len(), 2);
        assert_eq!(
            fs::read(fixture.destination.join(".env")).unwrap(),
            b"existing"
        );
        assert_eq!(
            fs::read_link(fixture.destination.join(".env.local")).unwrap(),
            PathBuf::from("missing")
        );
    }

    #[test]
    fn explicit_trust_is_project_local_and_exact_config_changes_revoke_it() {
        let fixture = Fixture::new();
        fs::write(fixture.primary.join(".env"), "secret").unwrap();
        let plan = fixture.config("copy = ['.env']\nsetup = 'printf done > setup-result'");
        let token = CancellationToken::new();
        assert!(!is_trusted(&fixture.primary, &plan, &token).unwrap());
        assert!(matches!(
            fixture.prepare(&plan),
            Err(SetupError::TrustRequired)
        ));
        assert!(!fixture.destination.join(".env").exists());
        trust(&fixture.primary, &plan, &token).unwrap();
        assert!(is_trusted(&fixture.primary, &plan, &token).unwrap());
        let preparation = fixture.prepare(&plan).unwrap();
        assert!(
            !fixture.destination.join("setup-result").exists(),
            "prepare must never run commands"
        );
        let output = Command::new("/bin/sh")
            .args(["-c", preparation.command.as_deref().unwrap()])
            .current_dir(&fixture.destination)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            fs::read(fixture.destination.join("setup-result")).unwrap(),
            b"done"
        );
        let other = Fixture::new();
        let same_plan = other.config("copy = ['.env']\nsetup = 'printf done > setup-result'");
        assert_eq!(plan.fingerprint, same_plan.fingerprint);
        assert!(!is_trusted(&other.primary, &same_plan, &token).unwrap());
        let changed = fixture.config("copy = ['.env']\nsetup = 'printf changed'");
        assert!(matches!(
            fixture.prepare(&plan),
            Err(SetupError::ConfigChanged)
        ));
        assert!(!is_trusted(&fixture.primary, &changed, &token).unwrap());
        assert!(matches!(
            trust(&fixture.primary, &plan, &token),
            Err(SetupError::ConfigChanged)
        ));
    }

    #[test]
    fn skip_has_no_io_and_cancel_keeps_created_checkout_usable() {
        let fixture = Fixture::new();
        let plan = fixture.config("setup = 'false'");
        let token = CancellationToken::new();
        token.cancel();
        let skipped = prepare(
            Path::new("/missing-primary"),
            Path::new("/missing-destination"),
            &plan,
            true,
            &token,
        )
        .unwrap();
        assert!(skipped.skipped);
        assert!(matches!(
            prepare(&fixture.primary, &fixture.destination, &plan, false, &token),
            Err(SetupError::Cancelled)
        ));
        assert!(fixture.destination.join("tracked").exists());
    }

    #[test]
    fn terminal_recipe_preserves_quotes_and_stops_after_failed_command() {
        let fixture = Fixture::new();
        let plan = fixture.config(
            "setup = [\"printf '%s' \\\"a'b\\\" > quoted\", 'false', 'touch should-not-exist']",
        );
        trust(&fixture.primary, &plan, &CancellationToken::new()).unwrap();
        let result = fixture.prepare(&plan).unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", result.command.as_deref().unwrap()])
            .current_dir(&fixture.destination)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            fs::read(fixture.destination.join("quoted")).unwrap(),
            b"a'b"
        );
        assert!(!fixture.destination.join("should-not-exist").exists());
        assert!(fixture.destination.join("tracked").exists());
    }

    #[test]
    fn inherited_git_environment_cannot_redirect_setup() {
        const CHILD_PRIMARY: &str = "CAGENTS_SETUP_TEST_PRIMARY";
        const CHILD_DESTINATION: &str = "CAGENTS_SETUP_TEST_DESTINATION";
        const MARKER: &str = "SETUP_ENV_BOUNDARY_VERIFIED";
        if let Some(primary) = std::env::var_os(CHILD_PRIMARY) {
            let primary = PathBuf::from(primary);
            let destination = PathBuf::from(std::env::var_os(CHILD_DESTINATION).unwrap());
            let plan = load(&primary).unwrap().unwrap();
            let cancel = CancellationToken::new();
            assert!(!is_trusted(&primary, &plan, &cancel).unwrap());
            assert!(matches!(
                prepare(&primary, &destination, &plan, false, &cancel),
                Err(SetupError::TrustRequired)
            ));
            // Explicit approval here must update this clone, not inherited GIT_DIR.
            trust(&primary, &plan, &cancel).unwrap();
            let prepared = prepare(&primary, &destination, &plan, false, &cancel).unwrap();
            assert_eq!(prepared.copied, [PathBuf::from(".env")]);
            assert_eq!(prepared.skipped_paths[0].path, PathBuf::from("tracked"));
            assert!(!destination.join("tracked").exists());
            println!("{MARKER}");
            return;
        }
        let fixture = Fixture::new();
        let other = Fixture::new();
        let config = "copy = ['.env', 'tracked']\nsetup = 'printf setup'";
        fixture.config(config);
        let other_plan = other.config(config);
        trust(&other.primary, &other_plan, &CancellationToken::new()).unwrap();
        fs::write(fixture.primary.join(".env"), "secret").unwrap();
        fs::write(fixture.primary.join("tracked"), "must not restore deleted tracked file").unwrap();
        fs::remove_file(fixture.destination.join("tracked")).unwrap();
        // Isolate environment mutation in a subprocess so other parallel tests
        // never inherit these intentionally hostile redirections.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worktree_setup::tests::inherited_git_environment_cannot_redirect_setup",
                "--nocapture",
            ])
            .env(CHILD_PRIMARY, &fixture.primary)
            .env(CHILD_DESTINATION, &fixture.destination)
            .env("GIT_DIR", other.primary.join(".git"))
            .env("GIT_WORK_TREE", &other.primary)
            .env("GIT_COMMON_DIR", other.primary.join(".git"))
            .env("GIT_INDEX_FILE", fixture._directory.path().join("missing-index"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(MARKER));
    }

    #[test]
    fn symlinked_config_cannot_supply_a_setup_plan() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.primary.join(".cagents")).unwrap();
        let external = fixture._directory.path().join("external.toml");
        fs::write(&external, "setup = 'false'").unwrap();
        symlink(&external, fixture.primary.join(CONFIG_PATH)).unwrap();
        assert!(load(&fixture.primary).is_err());
    }
}
