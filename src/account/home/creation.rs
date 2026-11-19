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
    #[cfg_attr(
        not(any(test, feature = "host")),
        expect(
            dead_code,
            reason = "only host enrollment reads the identity a receipt proves"
        )
    )]
    provider: Provider,
    #[cfg_attr(
        not(any(test, feature = "host")),
        expect(
            dead_code,
            reason = "only host enrollment reads the identity a receipt proves"
        )
    )]
    key: Uuid,
    path: PathBuf,
    parent_path: PathBuf,
    parent: fs::File,
    directory: fs::File,
}
impl NewHomeCreationReceipt {
    pub(crate) fn revalidate(&self) -> io::Result<()> {
        crate::secure_file::reject_symlink_components(&self.path)?;
        same_directory(&self.parent, &self.parent_path)?;
        same_directory(&self.directory, &self.path)
    }

    /// Removes the directory this receipt proves was made, once nothing has
    /// been written into it. The name is examined and removed relative to the
    /// parent descriptor held since `mkdirat`, so no path is resolved again,
    /// and only while that name still leads to the directory made. The
    /// kernel removes it only if it is empty. Between that check and the
    /// removal, another process of this user could put a different, empty
    /// directory under the name in the private parent; then that empty
    /// directory is what is removed. Contents are never removed: anything
    /// else is an error that leaves every file in place.
    #[cfg(any(test, feature = "host"))]
    pub(crate) fn withdraw(self) -> io::Result<()> {
        let name = std::ffi::CString::new(self.key.hyphenated().to_string())
            .expect("UUID has no zero byte");
        let held = self.directory.metadata()?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the held parent descriptor and CString outlive the call,
        // which initializes `named` when it succeeds.
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat succeeded, so it wrote the whole structure.
        let named = unsafe { named.assume_init() };
        if (named.st_dev, named.st_ino) != (held.dev(), held.ino()) {
            return Err(io::Error::other(
                "The newly created account directory changed; files were retained",
            ));
        }
        // SAFETY: as above. AT_REMOVEDIR removes only an empty directory.
        if unsafe { libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // The directory is gone either way. A failed sync risks only that a
        // crash brings back an empty, unlisted directory, which is no reason
        // to report the removal as retained files.
        if let Err(error) = self.parent.sync_all() {
            tracing::warn!(%error, "the withdrawn account directory's removal was not synced");
        }
        Ok(())
    }
}
/// What host enrollment and initializers read from a fresh home.
#[cfg(any(test, feature = "host"))]
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
    #[cfg(test)]
    pub(crate) fn parent(&self) -> &fs::File {
        &self.parent
    }
    pub(crate) fn directory(&self) -> &fs::File {
        &self.directory
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
        let root = crate::test_fixture::tempdir().unwrap();
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
        let root = crate::test_fixture::tempdir().unwrap();
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
    fn busy_enrollment_withdraws_only_the_untouched_directory_it_made() {
        let root = crate::test_fixture::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = crate::account::registry::Store::at(root.path().join("registry"));
        let mut created_path = None;
        let error = store
            .create_pending(&homes, Provider::Claude, 1, || {
                homes.create_with_config_initialized(
                    Provider::Claude,
                    None,
                    CodexConfig::Shared,
                    |created| {
                        created_path = Some(created.home().to_owned());
                        Err(EnrollmentRefused::Busy.into_error())
                    },
                )
            })
            .unwrap_err();
        assert!(
            matches!(EnrollmentRefused::of(&error), Some(EnrollmentRefused::Busy)),
            "{error}"
        );
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let path = created_path.unwrap();
        assert!(!path.exists(), "an untouched busy home is withdrawn");
        assert!(path.parent().unwrap().is_dir());
        assert!(store.load().unwrap().pending().is_empty());

        // A daemon that stopped admitting changes wrote nothing either.
        let mut closed_path = None;
        let error = homes
            .create_with_config_initialized(
                Provider::Claude,
                None,
                CodexConfig::Shared,
                |created| {
                    closed_path = Some(created.home().to_owned());
                    Err(EnrollmentRefused::Closed("closing".into()).into_error())
                },
            )
            .unwrap_err();
        assert!(matches!(
            EnrollmentRefused::of(&error),
            Some(EnrollmentRefused::Closed(message)) if message == "closing"
        ));
        assert!(!closed_path.unwrap().exists());

        // A directory with anything in it stays for recovery, as does a name
        // that no longer leads to the directory this call made.
        for replace in [false, true] {
            let mut kept = Vec::new();
            let error = homes
                .create_with_config_initialized(
                    Provider::Claude,
                    None,
                    CodexConfig::Shared,
                    |created| {
                        if replace {
                            let moved = created.home().with_extension("original");
                            fs::rename(created.home(), &moved)?;
                            fs::create_dir(created.home())?;
                            kept.extend([moved, created.home().to_owned()]);
                        } else {
                            fs::write(created.home().join("written"), "keep")?;
                            kept.push(created.home().join("written"));
                        }
                        Err(EnrollmentRefused::Busy.into_error())
                    },
                )
                .unwrap_err();
            assert!(EnrollmentRefused::of(&error).is_none());
            assert!(error.to_string().contains("files were retained"), "{error}");
            assert!(kept.iter().all(|path| path.exists()), "{kept:?}");
        }
    }
    #[test]
    fn replaced_child_or_parent_cannot_rebind_creation_receipt() {
        for replace_parent in [false, true] {
            let root = crate::test_fixture::tempdir().unwrap();
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
        let root = crate::test_fixture::tempdir().unwrap();
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
        let root = crate::test_fixture::tempdir().unwrap();
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
