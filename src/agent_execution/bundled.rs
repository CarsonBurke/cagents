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

const CONTENT: &str = include_str!("../../assets/skills/cagents-agents/SKILL.md");
const NAME: &str = "cagents-agents";
/// SHA-256 of every bundled `SKILL.md`, oldest first; the last entry is the
/// current [`CONTENT`]. An installed copy matching any entry is ours and
/// unedited, so it is upgraded in place; anything else is user content and is
/// never replaced. Append only: when `SKILL.md` changes, a test fails until its
/// new hash is appended, which keeps every earlier revision upgradable.
const BUNDLED: &[&str] = &[
    // 0e2d75a / #437: the first bundled revision.
    "9305357e17ee29e04d13494dc2a86a066684c3c7952c4f1bd4a45c577350308b",
    // e0bf6c0 / #546: documents --reasoning-effort.
    "7213ff9f5c012409711c3183c82f4dbb5293cbb309251334f83efda7fb863b33",
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
        if let Err(error) = install_at(&root, create_root) {
            tracing::warn!(path = %root.join("skills").join(NAME).join("SKILL.md").display(), %error,
                "cannot install bundled agent skill; cagents --json agent remains available");
        }
    }
}

fn install_at(root: &Path, create_root: bool) -> io::Result<()> {
    install_bundle(root, create_root, CONTENT.as_bytes(), BUNDLED)
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
/// `cagents-agents` directory or `SKILL.md` is a deliberately managed copy of
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
            std::fs::read_to_string(selected.join("skills/cagents-agents/SKILL.md")).unwrap(),
            CONTENT
        );
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".agents").exists());
        for provider in [Provider::Codex, Provider::Omp] {
            install(Some(provider), &environment, directory.path());
        }
        assert_eq!(
            std::fs::read_to_string(home.join(".agents/skills/cagents-agents/SKILL.md")).unwrap(),
            CONTENT
        );
    }

    #[test]
    fn user_content_is_preserved_and_removed_account_is_never_recreated() {
        let directory = tempfile::tempdir().unwrap();
        install_at(directory.path(), false).unwrap();
        let path = directory.path().join("skills/cagents-agents/SKILL.md");
        std::fs::write(&path, "user-authored skill").unwrap();
        assert_eq!(
            install_at(directory.path(), false).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
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
            "assets/skills/cagents-agents/SKILL.md changed. Append {actual} to BUNDLED \
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
