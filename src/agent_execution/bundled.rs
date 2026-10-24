//! Install the instruction-only bundle through provider-native skill discovery.
//! Callers hold the account lifecycle lease across installation and child spawn.
use crate::agent::Provider;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

const CONTENT: &str = include_str!("../../assets/skills/cagents/SKILL.md");
const NAME: &str = "cagents";
const LEGACY_NAME: &str = "cagents-agents";
/// SHA-256 of every bundled `SKILL.md`, oldest first; the last entry is the
/// current [`CONTENT`]. An installed copy matching any entry is ours and
/// unedited, so it is upgraded in place; anything else is user content and is
/// never replaced. Append only: when `SKILL.md` changes, a test fails until its
/// new hash is appended, which keeps every earlier revision upgradable.
const BUNDLED: &[&str] = &[
    "246f4a5f0a2d614ccf3ff204a967bcacf99cac23d257ceed87a2d994802b12a4",
    "91ef5d665ff6c64fef61cb960a70b6432f3e5aeec91de41ea20b5fc9f81add53",
    "da0498ddc44bae2ae3ea33e0dc7f8965c4289cb555accfce353461c3f221b75a",
];

/// Recognized copies of the retired cagents-agents skill, used only for cleanup.
const LEGACY_BUNDLED: &[&str] = &[
    // 0e2d75a / #437: the first bundled revision.
    "9305357e17ee29e04d13494dc2a86a066684c3c7952c4f1bd4a45c577350308b",
    // e0bf6c0 / #546: documents --reasoning-effort.
    "7213ff9f5c012409711c3183c82f4dbb5293cbb309251334f83efda7fb863b33",
    // #620: chooses between the harness and a hosted session; short handles.
    "6d81922a43f4a116c5ef9b55ab14fe1b6137f240037f88c31735e860653f4f57",
    // #621: messages between sessions.
    "95e462bbb60fe643b37cf76aacd642b87efa781c473f7723049e7f861b31b837",
    // #635: terminal commands change only the terminals a session controls.
    "0d7d6212da9aa810c004813d2d76b2f89233a77c4b40ba31ac737eeeb3bd09f5",
];
/// Larger than any bundled revision, so an oversized file is user content
/// without being read in full.
const READ_LIMIT: u64 = 1 << 20;

pub(crate) fn install(
    provider: Option<Provider>,
    environment: &BTreeMap<String, String>,
    cwd: &Path,
) {
    let Some(home) = environment
        .get("HOME")
        .map(|home| cwd.join(home))
        .filter(|home| home.is_dir())
    else {
        return;
    };
    let mut roots = Vec::new();
    if provider != Some(Provider::Claude) {
        // Both Codex and OMP document the user .agents skill provider. This
        // remains under the process HOME, including an explicitly overridden one.
        roots.push((home.join(".agents"), true));
    }
    if provider.is_none() || provider == Some(Provider::Claude) {
        let selected = environment
            .get("CLAUDE_CONFIG_DIR")
            .filter(|root| !root.is_empty());
        roots.push(match selected {
            Some(root) => (cwd.join(root), false),
            None => (home.join(".claude"), true),
        });
    }
    for (root, create_root) in roots {
        #[cfg(test)]
        if in_account_home(&root) {
            continue;
        }
        if !create_root
            && crate::account::home::Homes::installation()
                .and_then(|homes| homes.managed_identity(&root))
                .is_some_and(|(provider, _)| provider == Provider::Claude)
            && let Some(system) = crate::account::Account::system(Provider::Claude).resolved_home()
            && let Err(error) = share_managed_skills(&root, &system)
        {
            tracing::warn!(path = %root.display(), %error, "cannot share managed account skills");
        }
        if let Err(error) = install_at(&root, create_root) {
            tracing::warn!(path = %root.join("skills").join(NAME).join("SKILL.md").display(), %error,
                "cannot install bundled agent skill; cagents --json agent remains available");
        }
    }
}

/// Unit tests launch providers and shells with the developer's inherited
/// environment, `CLAUDE_CONFIG_DIR` included. Only fixture roots outside the
/// account's home may receive the bundle; an account that cannot be looked up
/// refuses every root.
#[cfg(test)]
fn in_account_home(root: &Path) -> bool {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0; 16 * 1024];
    let mut found = std::ptr::null_mut();
    // SAFETY: each pointer refers to live storage of the size given.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return true;
    }
    // SAFETY: a found entry is initialized and its strings live in `buffer`.
    let directory = unsafe { CStr::from_ptr(entry.assume_init_ref().pw_dir) };
    let account = Path::new(std::ffi::OsStr::from_bytes(directory.to_bytes()));
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    canonical(root).starts_with(canonical(account))
}

fn install_at(root: &Path, create_root: bool) -> io::Result<()> {
    let result = match install_bundle(root, create_root, CONTENT.as_bytes(), BUNDLED) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // A personal replacement is expected, not an installation failure.
            tracing::debug!(path = %root.join("skills").join(NAME).join("SKILL.md").display(), %error,
                "preserving personal agent skill");
            Ok(())
        }
        result => result,
    };
    // A personal cagents skill takes precedence over our fallback bundle, but
    // must not prevent retirement of the old bundle beside it.
    if root.is_dir() {
        retire_legacy(root)?;
    }
    result
}

/// Remove only the instruction file and directory we installed. Links, edits,
/// hard links, and directories with additional resources belong to the user.
fn retire_legacy(root: &Path) -> io::Result<()> {
    retire_bundle(root, LEGACY_NAME, LEGACY_BUNDLED)
}

/// Older homes may have been created before personal skills existed. Adopt the
/// live directory at launch once it exists, only when the account has no skills
/// of its own beyond recognized fallback bundles. Never retarget existing links.
fn share_managed_skills(root: &Path, system: &Path) -> io::Result<()> {
    let system = std::path::absolute(system)?;
    let source = system.join("skills");
    let destination = root.join("skills");
    if !root.is_dir() || !source.is_dir() || root.canonicalize()? == system.canonicalize()? {
        return Ok(());
    }
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_dir() => {
            retire_legacy(root)?;
            retire_bundle(root, NAME, BUNDLED)?;
            match fs::remove_dir(&destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(source, destination)
}

fn retire_bundle(root: &Path, name: &str, hashes: &[&str]) -> io::Result<()> {
    let skill = root.join("skills").join(name);
    let metadata = match fs::symlink_metadata(&skill) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(());
    }
    let directory_metadata = metadata;
    let entries = fs::read_dir(&skill)?.collect::<io::Result<Vec<_>>>()?;
    if entries.len() != 1 || entries[0].file_name() != "SKILL.md" {
        return Ok(());
    }
    let destination = skill.join("SKILL.md");
    let (content, metadata) = match read_regular(&destination, libc::O_NOFOLLOW) {
        Ok(value) => value,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP)
            || error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.nlink() != 1 || !hashes.contains(&sha256(&content).as_str()) {
        return Ok(());
    }
    // As with upgrades, narrow the window for a user edit or replacement
    // after the hash check. The launch lease serializes cagents launches, but
    // does not serialize edits made by the user.
    let current_directory = fs::symlink_metadata(&skill)?;
    let current = fs::symlink_metadata(&destination)?;
    if !current_directory.is_dir()
        || (current_directory.dev(), current_directory.ino())
            != (directory_metadata.dev(), directory_metadata.ino())
        || !current.is_file()
        || current.nlink() != 1
        || (current.dev(), current.ino(), current.len(), current.mtime(), current.mtime_nsec(), current.ctime(), current.ctime_nsec())
            != (metadata.dev(), metadata.ino(), metadata.len(), metadata.mtime(), metadata.mtime_nsec(), metadata.ctime(), metadata.ctime_nsec())
    {
        return Err(preserved("changed during retirement"));
    }
    fs::remove_file(destination)?;
    fs::remove_dir(skill)
}

fn install_bundle(
    root: &Path,
    create_root: bool,
    content: &[u8],
    previous: &[&str],
) -> io::Result<()> {
    if create_root {
        directory(root)?;
    }
    // Never create a selected account root. Account removal cannot be undone
    // by skill installation, including a missing linked configuration directory.
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "provider configuration root is absent",
        ));
    }
    let skills = root.join("skills");
    directory(&skills)?;
    let skill = skills.join(NAME);
    directory(&skill)?;
    let destination = skill.join("SKILL.md");
    match read_regular(&destination, 0) {
        Ok((existing, _)) if existing == content => return Ok(()),
        Ok(_) => return upgrade(&skill, &destination, content, previous),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut pending = tempfile::NamedTempFile::new_in(&skill)?;
    pending.write_all(content)?;
    pending.as_file().sync_all()?;
    match pending.persist_noclobber(&destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            // Two simultaneous launches may install the identical bundle.
            if fs::read(&destination)? == content {
                Ok(())
            } else {
                Err(error.error)
            }
        }
        Err(error) => Err(error.error),
    }
}

/// Replaces an unedited earlier bundle, and nothing else.
///
/// The hash allowlist is the guard: only a byte-exact earlier bundle, which
/// cagents itself wrote, is replaced. The provider root and its `skills`
/// directory may be links, as users commonly link them into dotfiles. A linked
/// `cagents` directory or `SKILL.md` is a deliberately managed copy of
/// this skill, so its target is never written even when it holds an earlier
/// bundle. The link checks are path-based, so a link swapped in between them
/// and the rename is not detected; the tree belongs to the daemon's user.
fn upgrade(skill: &Path, destination: &Path, content: &[u8], previous: &[&str]) -> io::Result<()> {
    if !fs::symlink_metadata(skill)?.file_type().is_dir() {
        return Err(preserved("directory is a symbolic link"));
    }
    let (existing, metadata) = read_regular(destination, libc::O_NOFOLLOW).map_err(|error| {
        match error.raw_os_error() {
            Some(libc::ELOOP) => preserved("is a symbolic link"),
            _ => error,
        }
    })?;
    if !previous.contains(&sha256(&existing).as_str()) {
        return Err(preserved(
            "differs from every bundled version and may be user-edited",
        ));
    }
    // A second hard link is another managed copy that a rename would detach.
    if metadata.nlink() > 1 {
        return Err(preserved("has another hard link"));
    }
    // Rename within the skill directory is atomic: a provider reading the
    // skill concurrently sees either the complete old or new revision.
    let mut pending = tempfile::NamedTempFile::new_in(skill)?;
    pending.write_all(content)?;
    pending
        .as_file()
        .set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() & 0o7777,
        ))?;
    pending.as_file().sync_all()?;
    // Narrow the window for an in-place edit made since the hash was checked.
    let current = fs::symlink_metadata(destination)?;
    if (
        current.ino(),
        current.len(),
        current.mtime(),
        current.mtime_nsec(),
    ) != (
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ) {
        return Err(preserved("changed during the upgrade"));
    }
    pending.persist(destination).map_err(|error| error.error)?;
    // The rename has happened; directory durability is best effort and must
    // not report a completed upgrade as a failed installation.
    if let Err(error) = fs::File::open(skill).and_then(|directory| directory.sync_all()) {
        tracing::debug!(%error, path = %skill.display(), "cannot sync upgraded skill directory");
    }
    Ok(())
}

fn preserved(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("existing skill {reason}; preserving it"),
    )
}

/// Reads at most [`READ_LIMIT`] + 1 bytes of a regular file. O_NONBLOCK keeps
/// a FIFO from stalling the launch; the descriptor's own metadata decides.
fn read_regular(path: &Path, flags: libc::c_int) -> io::Result<(Vec<u8>, fs::Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | flags)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(preserved("is not a regular file"));
    }
    let mut content = Vec::new();
    file.take(READ_LIMIT + 1).read_to_end(&mut content)?;
    Ok((content, metadata))
}

fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn directory(path: &Path) -> io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Use the CLI paired with this daemon, without persisting the installation
/// path into user-authored recipes or replacing the rest of their PATH.
pub(crate) fn matched_path(current: Option<&str>) -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    if !directory.join("cagents").is_file() {
        return None;
    }
    let mut paths = vec![directory.to_owned()];
    paths.extend(
        std::env::split_paths(current.unwrap_or_default()).filter(|path| path != directory),
    );
    std::env::join_paths(paths).ok()?.into_string().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "ui")]
    #[test]
    fn installed_bundle_is_discovered_by_each_native_provider_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let environment =
            BTreeMap::from([("HOME".into(), directory.path().to_str().unwrap().into())]);
        for provider in Provider::ALL {
            install(Some(provider), &environment, directory.path());
            let launch = crate::chat::Launch {
                provider,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: Vec::new(),
                    working_directory: directory.path().to_owned(),
                    environment: environment.clone(),
                    unset_environment: Default::default(),
                },
            };
            let skills = crate::chat::skills::discover(&launch);
            assert!(
                skills.iter().any(|skill| skill.name == NAME),
                "missing {provider:?} bundle: {skills:?}"
            );
        }
    }

    #[test]
    fn native_locations_follow_process_home_and_selected_claude_account() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("other-home");
        let selected = directory.path().join("selected-claude");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let environment = BTreeMap::from([
            ("HOME".into(), home.to_str().unwrap().into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                selected.to_str().unwrap().into(),
            ),
        ]);
        install(Some(Provider::Claude), &environment, directory.path());
        assert_eq!(
            std::fs::read_to_string(selected.join("skills/cagents/SKILL.md")).unwrap(),
            CONTENT
        );
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".agents").exists());
        for provider in [Provider::Codex, Provider::Omp] {
            install(Some(provider), &environment, directory.path());
        }
        assert_eq!(
            std::fs::read_to_string(home.join(".agents/skills/cagents/SKILL.md")).unwrap(),
            CONTENT
        );
    }

    #[test]
    fn user_content_is_preserved_and_removed_account_is_never_recreated() {
        let directory = tempfile::tempdir().unwrap();
        install_at(directory.path(), false).unwrap();
        let path = directory.path().join("skills/cagents/SKILL.md");
        std::fs::write(&path, "user-authored skill").unwrap();
        install_at(directory.path(), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "user-authored skill"
        );
        let removed = directory.path().join("removed");
        assert!(install_at(&removed, false).is_err());
        assert!(!removed.exists());
    }

    #[test]
    fn bundled_revision_hashes_are_recorded() {
        let actual = sha256(CONTENT.as_bytes());
        assert_eq!(
            BUNDLED.last(),
            Some(&actual.as_str()),
            "assets/skills/cagents/SKILL.md changed. Append {actual} to BUNDLED \
             and keep every earlier entry, so installed copies of them upgrade"
        );
        for (index, hash) in BUNDLED.iter().enumerate() {
            assert!(
                hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{hash} is not a lowercase SHA-256"
            );
            assert!(!BUNDLED[..index].contains(hash), "{hash} is repeated");
        }
    }

    const LEGACY: &[u8] = include_bytes!("../../fixtures/skills/retired-cagents-agents.md");

    fn legacy(root: &Path) -> std::path::PathBuf {
        let skill = root.join("skills").join(LEGACY_NAME);
        fs::create_dir_all(&skill).unwrap();
        let path = skill.join("SKILL.md");
        fs::write(&path, LEGACY).unwrap();
        path
    }

    #[test]
    fn managed_skills_adopt_late_personal_directory_and_follow_future_edits() {
        for previous in ["missing", "empty", "legacy", "current", "both"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("account");
            let system = directory.path().join("system");
            fs::create_dir(&root).unwrap();
            share_managed_skills(&root, &system).unwrap();
            assert!(!root.join("skills").exists());
            match previous {
                "missing" => {},
                "empty" => fs::create_dir(root.join("skills")).unwrap(),
                "legacy" => { legacy(&root); },
                "current" => install_at(&root, false).unwrap(),
                "both" => { install_at(&root, false).unwrap(); legacy(&root); },
                _ => unreachable!(),
            }
            let personal = system.join("skills/cagents/SKILL.md");
            fs::create_dir_all(personal.parent().unwrap()).unwrap();
            fs::write(&personal, b"personal revision one").unwrap();
            share_managed_skills(&root, &system).unwrap();
            assert_eq!(fs::read_link(root.join("skills")).unwrap(), system.join("skills"), "{previous}");
            fs::write(&personal, b"personal revision two").unwrap();
            assert_eq!(fs::read(installed(&root)).unwrap(), b"personal revision two");
            share_managed_skills(&root, &system).unwrap();
        }
    }

    #[test]
    fn managed_skills_resolve_relative_system_paths() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = directory.path().canonicalize().unwrap().join("account");
        let system = directory.path().join("system");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(system.join("skills")).unwrap();
        share_managed_skills(&root, &system).unwrap();
        assert!(fs::read_link(root.join("skills")).unwrap().is_absolute());
        assert_eq!(root.join("skills").canonicalize().unwrap(), system.join("skills").canonicalize().unwrap());
    }

    #[test]
    fn managed_skill_sharing_preserves_current_edits_resources_and_existing_links() {
        for case in ["edited", "resources", "hard-link", "skill-link", "skills-link", "dangling-link"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("account");
            let system = directory.path().join("system");
            fs::create_dir(&root).unwrap();
            fs::create_dir_all(system.join("skills")).unwrap();
            let path = installed(&root);
            let outside = directory.path().join("outside");
            if case == "skills-link" || case == "dangling-link" {
                if case == "skills-link" { fs::create_dir(&outside).unwrap(); }
                std::os::unix::fs::symlink(&outside, root.join("skills")).unwrap();
            } else {
                install_at(&root, false).unwrap();
                match case {
                    "edited" => fs::write(&path, b"account edit").unwrap(),
                    "resources" => fs::write(path.parent().unwrap().join("notes.md"), b"keep").unwrap(),
                    "hard-link" => fs::hard_link(&path, &outside).unwrap(),
                    "skill-link" => {
                        fs::rename(&path, &outside).unwrap();
                        std::os::unix::fs::symlink(&outside, &path).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            share_managed_skills(&root, &system).unwrap();
            if case == "skills-link" || case == "dangling-link" {
                assert_eq!(fs::read_link(root.join("skills")).unwrap(), outside);
            } else {
                assert!(fs::symlink_metadata(root.join("skills")).unwrap().is_dir(), "{case}");
                let expected = if case == "edited" { b"account edit".as_slice() } else { CONTENT.as_bytes() };
                assert_eq!(fs::read(&path).unwrap(), expected, "{case}");
            }
        }
    }

    #[test]
    fn managed_skill_sharing_preserves_account_customizations_and_removed_homes() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system");
        fs::create_dir_all(system.join("skills")).unwrap();
        let root = directory.path().join("removed");
        share_managed_skills(&root, &system).unwrap();
        assert!(!root.exists());
        fs::create_dir_all(root.join("skills/custom")).unwrap();
        fs::write(root.join("skills/custom/SKILL.md"), b"personal account skill").unwrap();
        share_managed_skills(&root, &system).unwrap();
        assert!(fs::symlink_metadata(root.join("skills")).unwrap().is_dir());
        assert_eq!(fs::read(root.join("skills/custom/SKILL.md")).unwrap(), b"personal account skill");
    }

    #[test]
    fn legacy_bundle_is_retired_and_current_bundle_is_installed() {
        let directory = tempfile::tempdir().unwrap();
        let path = legacy(directory.path());
        install_at(directory.path(), false).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert_eq!(fs::read(installed(directory.path())).unwrap(), CONTENT.as_bytes());
        install_at(directory.path(), false).unwrap();
    }

    #[test]
    fn personal_current_skill_is_preserved_while_legacy_bundle_is_retired() {
        let directory = tempfile::tempdir().unwrap();
        let path = legacy(directory.path());
        let current = installed(directory.path());
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, b"personal skill").unwrap();
        install_at(directory.path(), false).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert_eq!(fs::read(&current).unwrap(), b"personal skill");
    }

    #[test]
    fn legacy_edits_links_and_extra_resources_are_preserved() {
        for case in ["edited", "file-link", "directory-link", "hard-link", "resources", "fifo"] {
            let directory = tempfile::tempdir().unwrap();
            let path = legacy(directory.path());
            let outside = directory.path().join("outside");
            match case {
                "edited" => fs::write(&path, b"personal legacy skill").unwrap(),
                "file-link" => {
                    fs::rename(&path, &outside).unwrap();
                    std::os::unix::fs::symlink(&outside, &path).unwrap();
                }
                "directory-link" => {
                    fs::rename(path.parent().unwrap(), &outside).unwrap();
                    std::os::unix::fs::symlink(&outside, path.parent().unwrap()).unwrap();
                }
                "hard-link" => fs::hard_link(&path, &outside).unwrap(),
                "resources" => fs::write(path.parent().unwrap().join("notes.md"), b"keep").unwrap(),
                "fifo" => {
                    use std::os::unix::ffi::OsStrExt;
                    fs::remove_file(&path).unwrap();
                    let fifo = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: fifo is a valid NUL-terminated path.
                    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
                }
                _ => unreachable!(),
            }
            install_at(directory.path(), false).unwrap();
            assert!(fs::symlink_metadata(&path).is_ok(), "{case}");
            if case != "fifo" {
                let expected = if case == "edited" { b"personal legacy skill".as_slice() } else { LEGACY };
                assert_eq!(fs::read(&path).unwrap(), expected, "{case}");
            }
        }
    }

    const OLD: &[u8] = b"earlier bundled revision";
    const NEW: &[u8] = b"current bundled revision";

    fn installed(root: &Path) -> std::path::PathBuf {
        root.join("skills").join(NAME).join("SKILL.md")
    }

    fn assert_preserved(root: &Path, previous: &[&str]) {
        assert_eq!(
            install_bundle(root, false, NEW, previous)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn unedited_earlier_bundle_is_upgraded_in_place() {
        let directory = tempfile::tempdir().unwrap();
        install_bundle(directory.path(), false, OLD, &[]).unwrap();
        let path = installed(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let old = sha256(OLD);
        install_bundle(directory.path(), false, NEW, &[old.as_str()]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), NEW);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no temporary file is left beside the skill"
        );
        // An identical install is a no-op rather than another replacement.
        install_bundle(directory.path(), false, NEW, &[old.as_str()]).unwrap();
    }

    #[test]
    fn content_outside_the_allowlist_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        install_bundle(directory.path(), false, OLD, &[]).unwrap();
        let path = installed(directory.path());
        let mut edited = OLD.to_vec();
        edited.extend_from_slice(b"\nmy own note");
        fs::write(&path, &edited).unwrap();
        assert_preserved(directory.path(), &[sha256(OLD).as_str()]);
        assert_eq!(fs::read(&path).unwrap(), edited);
    }

    #[test]
    fn linked_skill_file_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        fs::create_dir(&root).unwrap();
        let managed = directory.path().join("dotfiles-SKILL.md");
        fs::write(&managed, OLD).unwrap();
        fs::create_dir_all(root.join("skills").join(NAME)).unwrap();
        let path = installed(&root);
        std::os::unix::fs::symlink(&managed, &path).unwrap();
        assert_preserved(&root, &[sha256(OLD).as_str()]);
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&managed).unwrap(), OLD);
    }

    #[test]
    fn linked_skill_directory_is_never_written() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let managed = directory.path().join("dotfiles").join(NAME);
        fs::create_dir_all(&managed).unwrap();
        fs::write(managed.join("SKILL.md"), OLD).unwrap();
        fs::create_dir_all(root.join("skills")).unwrap();
        std::os::unix::fs::symlink(&managed, root.join("skills").join(NAME)).unwrap();
        assert_preserved(&root, &[sha256(OLD).as_str()]);
        assert_eq!(fs::read(managed.join("SKILL.md")).unwrap(), OLD);
        assert_eq!(fs::read_dir(&managed).unwrap().count(), 1);
    }

    #[test]
    fn earlier_bundle_under_linked_root_and_skills_directory_is_upgraded() {
        let directory = tempfile::tempdir().unwrap();
        let dotfiles = directory.path().join("dotfiles");
        fs::create_dir_all(dotfiles.join("claude")).unwrap();
        fs::create_dir_all(dotfiles.join("skills").join(NAME)).unwrap();
        let skill = dotfiles.join("skills").join(NAME).join("SKILL.md");
        fs::write(&skill, OLD).unwrap();
        // ~/.claude -> dotfiles/claude and ~/.claude/skills -> ../skills.
        let root = directory.path().join(".claude");
        std::os::unix::fs::symlink(dotfiles.join("claude"), &root).unwrap();
        std::os::unix::fs::symlink("../skills", dotfiles.join("claude/skills")).unwrap();
        install_bundle(&root, false, NEW, &[sha256(OLD).as_str()]).unwrap();
        assert_eq!(fs::read(&skill).unwrap(), NEW);
        assert!(fs::symlink_metadata(&skill).unwrap().is_file());
        assert!(
            fs::symlink_metadata(root.join("skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // The allowlist still guards linked trees: an edited copy is kept.
        fs::write(&skill, b"edited").unwrap();
        assert_preserved(&root, &[sha256(OLD).as_str()]);
        assert_eq!(fs::read(&skill).unwrap(), b"edited");
    }

    #[test]
    fn hard_linked_skill_file_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        install_bundle(directory.path(), false, OLD, &[]).unwrap();
        let path = installed(directory.path());
        let managed = directory.path().join("managed-SKILL.md");
        fs::hard_link(&path, &managed).unwrap();
        assert_preserved(directory.path(), &[sha256(OLD).as_str()]);
        assert_eq!(fs::read(&path).unwrap(), OLD);
        assert_eq!(
            fs::metadata(&path).unwrap().ino(),
            fs::metadata(&managed).unwrap().ino()
        );
    }

    #[test]
    fn non_regular_skill_file_is_preserved_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        install_bundle(directory.path(), false, OLD, &[]).unwrap();
        let path = installed(directory.path());
        fs::remove_file(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path for this call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_preserved(directory.path(), &[sha256(OLD).as_str()]);
        assert!(!fs::symlink_metadata(&path).unwrap().is_file());
    }
}
