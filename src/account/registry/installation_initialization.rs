//! Identity-only writer. Existing unsafe entries are refused, never repaired.
use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

impl Store {
    pub(crate) fn try_initialize_installation_id(&self) -> io::Result<Uuid> {
        self.initialize_installation_with_io(|| Ok(()), |namespace| namespace.sync())
    }

    fn initialize_installation_with_io(
        &self,
        before_publish: impl FnOnce() -> io::Result<()>,
        sync: impl FnOnce(&super::super::durability::Directories) -> io::Result<()>,
    ) -> io::Result<Uuid> {
        let unsafe_entry = || io::Error::other("The installation metadata is unsafe");
        // Entry into this method is writer entry, including mkdir/lock effects.
        crate::secure_file::reject_symlink_components(&self.directory)?;
        match fs::symlink_metadata(&self.directory) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.directory)?;
            }
            Err(error) => return Err(error),
        }
        crate::secure_file::reject_symlink_components(&self.directory)?;
        crate::secure_file::validate_owned_directory(&self.directory)?;
        if fs::symlink_metadata(&self.directory)?.mode() & 0o077 != 0 {
            return Err(unsafe_entry());
        }
        let namespace = super::super::durability::Directories::capture(&self.directory)?;
        let lock_path = self.directory.join(LOCK_NAME);
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                crate::secure_file::open_existing_private_file_readonly(&lock_path)?
            }
            Err(error) => return Err(error),
        };
        let lock = crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB)?;
        let verify = || {
            namespace.revalidate()?;
            let held = lock.metadata()?;
            let named = fs::symlink_metadata(&lock_path)?;
            // SAFETY: geteuid has no preconditions or side effects.
            let uid = unsafe { libc::geteuid() };
            if !held.is_file()
                || !named.is_file()
                || held.uid() != uid
                || held.nlink() != 1
                || named.nlink() != 1
                || held.mode() & 0o077 != 0
                || named.mode() & 0o077 != 0
                || (held.dev(), held.ino()) != (named.dev(), named.ino())
            {
                return Err(unsafe_entry());
            }
            Ok(())
        };
        verify()?;
        let path = self.path();
        let existing = match crate::secure_file::open_existing_private_file_readonly(&path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let mut registry = if let Some(file) = &existing {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(unsafe_entry());
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| unsafe_entry())?;
            let registry: Registry = toml::from_str(text).map_err(|_| unsafe_entry())?;
            registry.validate_preparations()?;
            registry
        } else {
            Registry::default()
        };
        if registry.is_newer() {
            return Err(unsafe_entry());
        }
        let verify_record = || {
            verify()?;
            match (&existing, fs::symlink_metadata(&path)) {
                (Some(file), Ok(named)) => {
                    let held = file.metadata()?;
                    if !named.is_file()
                        || named.nlink() != 1
                        || named.mode() & 0o077 != 0
                        || (held.dev(), held.ino()) != (named.dev(), named.ino())
                    {
                        return Err(unsafe_entry());
                    }
                }
                (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(unsafe_entry()),
            }
            Ok(())
        };
        if let Some(id) = registry.installation {
            verify_record()?;
            sync(&namespace)?;
            verify_record()?;
            return Ok(id);
        }
        let id = Uuid::new_v4();
        registry.installation = Some(id);
        registry.version = VERSION;
        registry.revision = registry.revision.checked_add(1).ok_or_else(unsafe_entry)?;
        write_with_io(
            &self.directory,
            &path,
            &registry,
            || {
                before_publish()?;
                verify_record()
            },
            |_| {
                verify()?;
                sync(&namespace)?;
                verify()
            },
        )?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn initialization_preserves_legacy_rows_and_existing_identity_repeats_only_the_barrier() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        store
            .update(|registry| registry.begin(Provider::Codex, Uuid::from_u128(70), 0))
            .unwrap();
        let before = store.load().unwrap();
        let id = store.try_initialize_installation_id().unwrap();
        assert!(!id.is_nil());
        let mut after = store.load().unwrap();
        assert_eq!(after.installation, Some(id));
        after.installation = before.installation;
        after.revision = before.revision;
        after.version = before.version;
        assert_eq!(
            after, before,
            "identity initialization preserves every existing row/option"
        );
        let bytes = fs::read(store.path()).unwrap();
        let inode = fs::metadata(store.path()).unwrap().ino();
        let count = std::cell::Cell::new(0);
        assert_eq!(
            store
                .initialize_installation_with_io(
                    || panic!("unchanged identity published"),
                    |namespace| {
                        count.set(count.get() + 1);
                        namespace.sync()
                    }
                )
                .unwrap(),
            id
        );
        assert_eq!(count.get(), 1);
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        assert_eq!(fs::metadata(store.path()).unwrap().ino(), inode);
    }

    #[test]
    fn initialization_never_confirms_failed_publish_or_existing_identity_barriers() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        assert!(
            store
                .initialize_installation_with_io(
                    || Err(io::Error::other("before publish")),
                    |_| Ok(())
                )
                .is_err()
        );
        assert!(!store.path().exists());
        assert!(
            store
                .initialize_installation_with_io(
                    || Ok(()),
                    |_| Err(io::Error::other("after rename"))
                )
                .is_err()
        );
        let visible = store.inspect_installation_id().unwrap().unwrap();
        assert!(
            store
                .initialize_installation_with_io(
                    || panic!("must not republish"),
                    |_| Err(io::Error::other("unchanged barrier"))
                )
                .is_err()
        );
        assert_eq!(store.try_initialize_installation_id().unwrap(), visible);
    }

    #[test]
    fn initialization_refuses_unsafe_existing_entries_without_permission_repair() {
        for kind in [
            "directory",
            "file",
            "hardlink",
            "symlink",
            "lock",
            "lock-hardlink",
            "corrupt",
            "newer",
            "nil",
        ] {
            let root = tempfile::tempdir().unwrap();
            let store = Store::at(root.path().join("registry"));
            fs::create_dir(&store.directory).unwrap();
            fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o700)).unwrap();
            let file = if kind.starts_with("lock") {
                store.directory.join(LOCK_NAME)
            } else {
                store.path()
            };
            fs::write(
                &file,
                match kind {
                    "corrupt" => "private-invalid=[",
                    "newer" => "version=7\n",
                    "nil" => "version=6\ninstallation='00000000-0000-0000-0000-000000000000'\n",
                    _ => "version=6\n",
                },
            )
            .unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            if kind == "directory" {
                fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o755)).unwrap();
            }
            if matches!(kind, "file" | "lock") {
                fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            }
            if matches!(kind, "hardlink" | "lock-hardlink") {
                fs::hard_link(&file, root.path().join("alias")).unwrap();
            }
            if kind == "symlink" {
                fs::rename(&file, root.path().join("original")).unwrap();
                std::os::unix::fs::symlink(root.path().join("original"), &file).unwrap();
            }
            let bytes = fs::read(&file).unwrap();
            let mode = fs::metadata(&file).unwrap().mode();
            let directory_mode = fs::metadata(&store.directory).unwrap().mode();
            assert!(store.try_initialize_installation_id().is_err(), "{kind}");
            assert_eq!(fs::read(&file).unwrap(), bytes, "{kind}");
            assert_eq!(fs::metadata(&file).unwrap().mode(), mode, "{kind}");
            assert_eq!(
                fs::metadata(&store.directory).unwrap().mode(),
                directory_mode,
                "{kind}"
            );
        }
    }

    #[test]
    fn initialization_retains_original_namespace_and_refuses_a_replacement_before_publish() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        let original = root.path().join("original");
        assert!(
            store
                .initialize_installation_with_io(
                    || {
                        fs::rename(&store.directory, &original)?;
                        fs::create_dir(&store.directory)?;
                        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o700))
                    },
                    |namespace| namespace.sync()
                )
                .is_err()
        );
        assert!(!store.path().exists());
        assert!(!original.join(FILE_NAME).exists());
        assert!(original.join(LOCK_NAME).is_file());
    }

    #[test]
    fn initialization_converges_after_concurrent_contention_without_waiting_or_mint_receipts() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        let first = store.clone();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            first.initialize_installation_with_io(
                || {
                    entered.send(()).unwrap();
                    resume
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    Ok(())
                },
                |namespace| namespace.sync(),
            )
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let reply = crate::account::installation::initialize(
            crate::account::installation::InitializationRequest {
                request_id: Uuid::from_u128(501),
            },
            || Some(store.clone()),
        );
        assert_eq!(
            reply.outcome,
            crate::account::installation::InitializationOutcome::Unknown
        );
        release.send(()).unwrap();
        let id = worker.join().unwrap().unwrap();
        let bytes = fs::read(store.path()).unwrap();
        assert_eq!(store.try_initialize_installation_id().unwrap(), id);
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
    }

    #[test]
    fn initialization_existing_identity_never_confirms_a_replaced_namespace() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("registry"));
        store.try_initialize_installation_id().unwrap();
        let original = root.path().join("original");
        assert!(
            store
                .initialize_installation_with_io(
                    || panic!("no publication"),
                    |namespace| {
                        fs::rename(&store.directory, &original)?;
                        fs::create_dir(&store.directory)?;
                        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o700))?;
                        namespace.sync()
                    }
                )
                .is_err()
        );
        assert!(!store.path().exists());
        assert!(original.join(FILE_NAME).is_file());
    }
}
