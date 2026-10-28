//! Proof of one successful, unpublished managed-home creation.
use super::*;
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

/// A borrowing initializer receives the original descriptors, never a receipt
/// reconstructed from an existing directory or an "empty" observation. This
/// is creation provenance only; it does not grant provider/history read access.
/// Deliberately has no Clone, Deserialize or public constructor.
pub(crate) struct NewHomeCreationReceipt {
    provider: Provider,
    key: Uuid,
    path: PathBuf,
    parent_path: PathBuf,
    parent: fs::File,
    directory: fs::File,
}
impl NewHomeCreationReceipt {
    pub(crate) fn provider(&self) -> Provider {
        self.provider
    }
    pub(crate) fn key(&self) -> Uuid {
        self.key
    }
    pub(crate) fn home(&self) -> &Path {
        &self.path
    }
    pub(crate) fn parent(&self) -> &fs::File {
        &self.parent
    }
    pub(crate) fn directory(&self) -> &fs::File {
        &self.directory
    }
    pub(crate) fn revalidate(&self) -> io::Result<()> {
        crate::secure_file::reject_symlink_components(&self.path)?;
        same_directory(&self.parent, &self.parent_path)?;
        same_directory(&self.directory, &self.path)
    }
}
fn same_directory(held: &fs::File, path: &Path) -> io::Result<()> {
    let held = held.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !held.is_dir()
        || !named.is_dir()
        || held.uid() != unsafe { libc::geteuid() }
        || named.uid() != held.uid()
        || held.mode() & 0o077 != 0
        || named.mode() & 0o077 != 0
        || (held.dev(), held.ino()) != (named.dev(), named.ino())
    {
        return Err(io::Error::other(
            "The newly created account directory changed; files were retained",
        ));
    }
    Ok(())
}
/// This is the only receipt mint. Success requires this exact nonrecursive
/// creation to succeed; EEXIST is never converted into an adopt-existing path.
pub(super) fn create(
    parent: &Path,
    provider: Provider,
    key: Uuid,
) -> io::Result<NewHomeCreationReceipt> {
    crate::secure_file::reject_symlink_components(parent)?;
    crate::secure_file::validate_owned_directory(parent)?;
    let parent_path = parent.canonicalize()?;
    let parent = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&parent_path)?;
    same_directory(&parent, &parent_path)?;
    let name = key.hyphenated().to_string();
    let c_name = std::ffi::CString::new(name.as_bytes()).expect("UUID has no zero byte");
    // SAFETY: the retained parent and CString outlive mkdirat. No recursive
    // creation or existing directory is accepted as a new home.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat yields one newly owned descriptor. Its path
    // is the exact fresh UUID beneath the parent pinned before mkdirat.
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = unsafe { fs::File::from_raw_fd(raw) };
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    directory.sync_all()?;
    parent.sync_all()?;
    let created = NewHomeCreationReceipt {
        provider,
        key,
        path: parent_path.join(name),
        parent_path,
        parent,
        directory,
    };
    created.revalidate()?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_precedes_seed_links_and_registry_publication_and_uses_actual_fds() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let system = root.path().join("system");
        fs::create_dir(&system).unwrap();
        fs::write(system.join("settings.json"), r#"{"fixture":true}"#).unwrap();
        fs::write(system.join("CLAUDE.md"), "fixture instructions").unwrap();
        let store = crate::account::registry::Store::at(root.path().join("registry"));
        let mut captured = None;
        let result = store
            .create_pending(&homes, Provider::Claude, 1, || {
                homes.create_with_config_initialized(
                    Provider::Claude,
                    Some(&system),
                    CodexConfig::Shared,
                    |created| {
                        created.revalidate()?;
                        assert_eq!(created.provider(), Provider::Claude);
                        assert_eq!(
                            created.home().file_name(),
                            Some(std::ffi::OsStr::new(&created.key().to_string()))
                        );
                        assert_eq!(
                            created.directory().metadata()?.ino(),
                            created.home().metadata()?.ino()
                        );
                        assert_eq!(
                            created.parent().metadata()?.ino(),
                            created.home().parent().unwrap().metadata()?.ino()
                        );
                        assert!(
                            fs::read_dir(created.home())?.next().is_none(),
                            "seed and shared links must follow initialization"
                        );
                        assert!(
                            !root.path().join("registry/accounts.toml").exists(),
                            "receipt precedes registry publication"
                        );
                        captured = Some((created.key(), created.home().to_owned()));
                        fs::write(created.home().join("bootstrap-fixture"), "initialized")
                    },
                )
            })
            .unwrap();
        assert_eq!(Some(result.clone()), captured);
        assert_eq!(store.load().unwrap().pending().len(), 1);
        assert_eq!(
            fs::read_to_string(result.1.join("settings.json")).unwrap(),
            r#"{"fixture":true}"#
        );
        assert!(
            fs::symlink_metadata(result.1.join("CLAUDE.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(result.1.join("bootstrap-fixture")).unwrap(),
            "initialized"
        );
    }
    #[test]
    fn initializer_failure_retains_files_and_never_publishes_or_seeds() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let system = root.path().join("system");
        fs::create_dir(&system).unwrap();
        fs::write(system.join("settings.json"), "{}").unwrap();
        let store = crate::account::registry::Store::at(root.path().join("registry"));
        let mut created_path = None;
        let result = store.create_pending(&homes, Provider::Claude, 1, || {
            homes.create_with_config_initialized(
                Provider::Claude,
                Some(&system),
                CodexConfig::Shared,
                |created| {
                    created_path = Some(created.home().to_owned());
                    fs::write(created.home().join("retained"), "do not roll back")?;
                    Err(io::Error::other("fixture initialization failure"))
                },
            )
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("files were retained")
        );
        let path = created_path.unwrap();
        assert_eq!(
            fs::read_to_string(path.join("retained")).unwrap(),
            "do not roll back"
        );
        assert!(!path.join("settings.json").exists());
        assert!(store.load().unwrap().pending().is_empty());
    }
    #[test]
    fn replaced_child_or_parent_cannot_rebind_creation_receipt() {
        for replace_parent in [false, true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let homes = Homes::at(root.path().join("homes"));
            let mut old_path = None;
            let result = homes.create_with_config_initialized(
                Provider::Claude,
                None,
                CodexConfig::Shared,
                |created| {
                    fs::write(created.home().join("original"), "keep")?;
                    if replace_parent {
                        let parent = created.home().parent().unwrap();
                        let moved = parent.with_extension("original");
                        fs::rename(parent, &moved)?;
                        fs::create_dir(parent)?;
                        fs::create_dir(parent.join(created.home().file_name().unwrap()))?;
                        old_path = Some(moved.join(created.home().file_name().unwrap()));
                    } else {
                        let moved = created.home().with_extension("original");
                        fs::rename(created.home(), &moved)?;
                        fs::create_dir(created.home())?;
                        old_path = Some(moved);
                    }
                    assert!(created.revalidate().is_err());
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(
                fs::read_to_string(old_path.unwrap().join("original")).unwrap(),
                "keep"
            );
        }
    }
    #[test]
    fn existing_directory_never_yields_a_creation_receipt() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let key = Uuid::new_v4();
        let created = create(root.path(), Provider::Claude, key).unwrap();
        assert!(fs::read_dir(created.home()).unwrap().next().is_none());
        assert!(
            create(root.path(), Provider::Claude, key).is_err(),
            "even an empty existing home is never adopted"
        );
        fs::write(created.home().join("existing"), "keep").unwrap();
        assert!(create(root.path(), Provider::Claude, key).is_err());
        assert_eq!(
            fs::read_to_string(created.home().join("existing")).unwrap(),
            "keep"
        );
    }
    #[test]
    fn widened_private_directory_permissions_refuse_publication_and_retain_files() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let mut path = None;
        let result = homes.create_with_config_initialized(
            Provider::Claude,
            None,
            CodexConfig::Shared,
            |created| {
                path = Some(created.home().to_owned());
                fs::write(created.home().join("retained"), "keep")?;
                created
                    .directory()
                    .set_permissions(fs::Permissions::from_mode(0o755))?;
                assert!(created.revalidate().is_err());
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(path.unwrap().join("retained")).unwrap(),
            "keep"
        );
    }
}
