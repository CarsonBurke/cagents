//! Exclusive creation of one fixed, independent Codex configuration.
//!
//! This module grants no account or execution authority. The caller must hold
//! the selected home's lifecycle reservation, and `publish` requires the Store's
//! actual registry lease throughout both revalidations and publication. Its
//! callback must check the selected registration/original home and the full
//! latest registry/System home AND fixed config-target alias census, including
//! dangling links from another home to the absent selected config.toml.
//! A diagnostic is not a permit.
//! No existing configuration, credential, home permission or registry is read
//! for its contents or changed here. There is no named-temporary fallback.

use std::{
    ffi::OsStr,
    fs::{File, Metadata, Permissions},
    io::{self, Read, Seek, SeekFrom, Write},
    mem::MaybeUninit,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

use serde::{Deserialize, Serialize};

use crate::fs_safety::{self, Root};

const NAME: &str = "config.toml";
const OFFLINE: &[u8] = b"approval_policy = \"on-request\"\nsandbox_mode = \"workspace-write\"\n\n[sandbox_workspace_write]\nnetwork_access = false\n";
const ONLINE: &[u8] = b"approval_policy = \"on-request\"\nsandbox_mode = \"workspace-write\"\n\n[sandbox_workspace_write]\nnetwork_access = true\n";

/// Metadata only. Read-only/implicit provider policy does not establish absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Presence {
    Absent,
    Present,
    Unknown,
}

/// Fixed failure projections; never paths, OS diagnostics or configuration text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    HomeUnavailable,
    ExistingConfiguration,
    PresenceUnknown,
    StagingUnavailable,
    InvalidStagedFile,
    WriteFailed,
    VerificationFailed,
    FileSyncFailed,
    ContextChanged,
    PublicationUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Uncertainty {
    DirectorySyncFailed,
    ContextChanged,
    PublishedFileChanged,
}

/// Created records this fixed publication, not effective policy or readiness.
/// An uncertain visible file is retained; it is never unlinked as rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Created,
    NotCreated(Refusal),
    CreationUncertain(Uncertainty),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}
impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn entry(root: &Root) -> io::Result<libc::stat> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: Root retains a directory descriptor; the basename is fixed and
    // NUL-terminated, and fstatat initializes stat only on success. NOFOLLOW
    // classifies dangling symlinks as present instead of inspecting targets.
    let result = unsafe {
        libc::fstatat(
            root.descriptor(),
            c"config.toml".as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: fstatat succeeded above and initialized the structure.
        Ok(unsafe { metadata.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn presence(root: &Root) -> Presence {
    match entry(root) {
        Ok(_) => Presence::Present,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Presence::Absent,
        Err(_) => Presence::Unknown,
    }
}

fn owned_directory(root: &Root) -> Result<Identity, Refusal> {
    let metadata = root.metadata().map_err(|_| Refusal::HomeUnavailable)?;
    // SAFETY: geteuid has no preconditions and does not change process state.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.nlink() == 0 {
        return Err(Refusal::HomeUnavailable);
    }
    Ok(Identity::of(&metadata))
}

fn owned_file(file: &File, links: u64) -> Result<Metadata, Refusal> {
    let metadata = file.metadata().map_err(|_| Refusal::InvalidStagedFile)?;
    // SAFETY: geteuid has no preconditions and does not change process state.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != uid || metadata.nlink() != links {
        return Err(Refusal::InvalidStagedFile);
    }
    Ok(metadata)
}

fn verify(file: &mut File, expected: &[u8], links: u64) -> Result<(), Refusal> {
    let metadata = owned_file(file, links)?;
    if metadata.mode() & 0o7777 != 0o600 || metadata.len() != expected.len() as u64 {
        return Err(Refusal::VerificationFailed);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| Refusal::VerificationFailed)?;
    let mut actual = Vec::with_capacity(expected.len() + 1);
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)
        .map_err(|_| Refusal::VerificationFailed)?;
    if actual != expected {
        return Err(Refusal::VerificationFailed);
    }
    Ok(())
}

/// Owns one unnamed inode and its original directory, never a mutable source
/// pathname. Dropping before publication naturally retires the unnamed inode.
pub(crate) struct Prepared {
    root: Root,
    home: Identity,
    file: File,
    expected: &'static [u8],
}

pub(crate) fn prepare(root: &Root, network_access: bool) -> Result<Prepared, Refusal> {
    prepare_with(root, network_access, |root| {
        fs_safety::create_anonymous_in(root, 0o600).map_err(|_| Refusal::StagingUnavailable)
    })
}

fn prepare_with(
    root: &Root,
    network_access: bool,
    create: impl FnOnce(&Root) -> Result<File, Refusal>,
) -> Result<Prepared, Refusal> {
    let home = owned_directory(root)?;
    match presence(root) {
        Presence::Present => return Err(Refusal::ExistingConfiguration),
        Presence::Unknown => return Err(Refusal::PresenceUnknown),
        Presence::Absent => {}
    }
    let root = root.try_clone().map_err(|_| Refusal::HomeUnavailable)?;
    let mut file = create(&root)?;
    // Before chmod, prove this is our still-unnamed inode. Passing mode0600
    // at creation is insufficient under a restrictive inherited umask.
    owned_file(&file, 0)?;
    file.set_permissions(Permissions::from_mode(0o600))
        .map_err(|_| Refusal::InvalidStagedFile)?;
    if owned_file(&file, 0)?.mode() & 0o7777 != 0o600 {
        return Err(Refusal::InvalidStagedFile);
    }
    let expected = if network_access { ONLINE } else { OFFLINE };
    file.write_all(expected).map_err(|_| Refusal::WriteFailed)?;
    verify(&mut file, expected, 0)?;
    file.sync_all().map_err(|_| Refusal::FileSyncFailed)?;
    Ok(Prepared {
        root,
        home,
        file,
        expected,
    })
}

impl Prepared {
    /// Caller holds the real Store registry lease and lifecycle reservation
    /// throughout this call. Revalidation checks full home/config-target alias
    /// censuses and exact selected context; it is not an execution permit.
    /// Before-link refusal means no publication; after-link failure is uncertain.
    pub(crate) fn publish(self, revalidate: impl FnMut() -> bool) -> Outcome {
        self.publish_with(
            revalidate,
            |root, file| fs_safety::link_anonymous_new(root, file, OsStr::new(NAME)),
            fs_safety::sync_directory,
        )
    }

    fn publish_with(
        mut self,
        mut revalidate: impl FnMut() -> bool,
        publish: impl FnOnce(&Root, &File) -> Result<(), fs_safety::ResolveError>,
        sync: impl FnOnce(&Root) -> io::Result<()>,
    ) -> Outcome {
        if owned_directory(&self.root) != Ok(self.home) {
            return Outcome::NotCreated(Refusal::HomeUnavailable);
        }
        if let Err(reason) = verify(&mut self.file, self.expected, 0) {
            return Outcome::NotCreated(reason);
        }
        match presence(&self.root) {
            Presence::Present => return Outcome::NotCreated(Refusal::ExistingConfiguration),
            Presence::Unknown => return Outcome::NotCreated(Refusal::PresenceUnknown),
            Presence::Absent => {}
        }
        if !revalidate() {
            return Outcome::NotCreated(Refusal::ContextChanged);
        }
        if let Err(error) = publish(&self.root, &self.file) {
            return Outcome::NotCreated(if error.is_already_exists() {
                Refusal::ExistingConfiguration
            } else {
                // This existing primitive's failed link leaves our inode
                // unnamed. Unsupported and permission errors remain refusals;
                // no named fallback or inferred provider diagnosis is used.
                Refusal::PublicationUnavailable
            });
        }
        // From here onward the file is visible. Even a failed fsync/context
        // check must not unlink a configuration another client may now use.
        let synced = sync(&self.root).is_ok();
        let current = revalidate() && owned_directory(&self.root) == Ok(self.home);
        let file = self.file.metadata();
        let destination = entry(&self.root);
        let same_entry = match (file, destination) {
            (Ok(file), Ok(destination)) => {
                file.dev() == destination.st_dev as u64 && file.ino() == destination.st_ino as u64
            }
            _ => false,
        };
        let intact = same_entry && verify(&mut self.file, self.expected, 1).is_ok();
        if !synced {
            Outcome::CreationUncertain(Uncertainty::DirectorySyncFailed)
        } else if !current {
            Outcome::CreationUncertain(Uncertainty::ContextChanged)
        } else if !intact {
            Outcome::CreationUncertain(Uncertainty::PublishedFileChanged)
        } else {
            Outcome::Created
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink, process::Command};

    fn fixture() -> (tempfile::TempDir, Root) {
        let directory = tempfile::tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap();
        (directory, root)
    }

    fn unavailable(root: &Root) -> fs_safety::ResolveError {
        fs_safety::ResolveError::Io {
            path: root.path().to_owned(),
            message: "fixture unavailable".into(),
            not_found: false,
            already_exists: false,
            write_refused: false,
        }
    }

    #[test]
    fn absence_does_not_follow_or_touch_any_existing_entry_type() {
        for kind in ["file", "directory", "symlink", "dangling", "hardlink"] {
            let (directory, root) = fixture();
            assert_eq!(presence(&root), Presence::Absent);
            let target = directory.path().join(NAME);
            let original = directory.path().join("original");
            fs::write(&original, b"preserve this fixture").unwrap();
            match kind {
                "file" => fs::write(&target, b"existing configuration").unwrap(),
                "directory" => fs::create_dir(&target).unwrap(),
                "symlink" => symlink(&original, &target).unwrap(),
                "dangling" => symlink(directory.path().join("missing"), &target).unwrap(),
                "hardlink" => fs::hard_link(&original, &target).unwrap(),
                _ => unreachable!(),
            }
            let identity = Identity::of(&fs::symlink_metadata(&target).unwrap());
            assert_eq!(presence(&root), Presence::Present);
            assert!(matches!(
                prepare(&root, false),
                Err(Refusal::ExistingConfiguration)
            ));
            assert!(Identity::of(&fs::symlink_metadata(&target).unwrap()) == identity);
            assert_eq!(fs::read(&original).unwrap(), b"preserve this fixture");
        }
    }

    #[test]
    fn fixed_network_choice_publishes_complete_independent_private_configuration() {
        for network in [false, true] {
            let (directory, root) = fixture();
            let prepared = prepare(&root, network).expect("anonymous staging must qualify");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
            let mut checks = 0;
            assert_eq!(
                prepared.publish(|| {
                    checks += 1;
                    true
                }),
                Outcome::Created
            );
            assert_eq!(checks, 2);
            let path = directory.path().join(NAME);
            let metadata = fs::metadata(&path).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o600);
            assert_eq!(metadata.nlink(), 1);
            // Only the requested profile fields are allowed; extra sandbox
            // overrides must not silently change standard workspace behavior.
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Profile {
                approval_policy: String,
                sandbox_mode: String,
                sandbox_workspace_write: WorkspaceWrite,
            }
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct WorkspaceWrite {
                network_access: bool,
            }
            let profile: Profile = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(profile.approval_policy, "on-request");
            assert_eq!(profile.sandbox_mode, "workspace-write");
            assert_eq!(profile.sandbox_workspace_write.network_access, network);
        }
    }

    #[test]
    fn a_creator_between_final_validation_and_link_wins_without_replacement() {
        let (directory, root) = fixture();
        let prepared = prepare(&root, true).expect("anonymous staging must qualify");
        let path = directory.path().join(NAME);
        let outcome = prepared.publish(|| {
            fs::write(&path, b"concurrent user's configuration").unwrap();
            true
        });
        assert_eq!(outcome, Outcome::NotCreated(Refusal::ExistingConfiguration));
        assert_eq!(fs::read(path).unwrap(), b"concurrent user's configuration");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn cancellation_or_failed_staging_never_publishes_or_leaves_a_temporary_name() {
        let (directory, root) = fixture();
        assert!(matches!(
            prepare_with(&root, false, |_| Err(Refusal::StagingUnavailable)),
            Err(Refusal::StagingUnavailable)
        ));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        let prepared = prepare(&root, false).expect("anonymous staging must qualify");
        assert_eq!(
            prepared.publish(|| false),
            Outcome::NotCreated(Refusal::ContextChanged)
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        drop(prepare(&root, false).expect("anonymous staging must qualify"));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_named_staging_substitution_is_refused_before_chmod_or_contents_change() {
        let (directory, root) = fixture();
        let path = directory.path().join("existing");
        fs::write(&path, b"leave this alone").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        assert!(matches!(
            prepare_with(&root, false, |_| Ok(file)),
            Err(Refusal::InvalidStagedFile)
        ));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(fs::read(&path).unwrap(), b"leave this alone");
        assert_eq!(presence(&root), Presence::Absent);
    }

    #[test]
    fn failed_publication_is_a_fixed_refusal_without_a_named_fallback() {
        let (directory, root) = fixture();
        let prepared = prepare(&root, false).expect("anonymous staging must qualify");
        let outcome = prepared.publish_with(
            || true,
            |root, _| Err(unavailable(root)),
            |_| panic!("no directory sync before publication"),
        );
        assert_eq!(
            outcome,
            Outcome::NotCreated(Refusal::PublicationUnavailable)
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(!serde_json::to_string(&outcome).unwrap().contains("fixture"));
    }

    #[test]
    fn directory_sync_or_late_context_failure_preserves_the_visible_file_as_uncertain() {
        for fail_sync in [false, true] {
            let (directory, root) = fixture();
            let prepared = prepare(&root, false).expect("anonymous staging must qualify");
            let mut checks = 0;
            let outcome = prepared.publish_with(
                || {
                    checks += 1;
                    checks == 1
                },
                |root, file| fs_safety::link_anonymous_new(root, file, OsStr::new(NAME)),
                |root| {
                    if fail_sync {
                        Err(io::Error::other("fixture sync failure"))
                    } else {
                        fs_safety::sync_directory(root)
                    }
                },
            );
            assert_eq!(
                outcome,
                Outcome::CreationUncertain(if fail_sync {
                    Uncertainty::DirectorySyncFailed
                } else {
                    Uncertainty::ContextChanged
                })
            );
            assert_eq!(checks, 2);
            assert_eq!(fs::read(directory.path().join(NAME)).unwrap(), OFFLINE);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn a_replaced_published_entry_is_preserved_and_cannot_report_created() {
        let (directory, root) = fixture();
        let prepared = prepare(&root, true).expect("anonymous staging must qualify");
        let path = directory.path().join(NAME);
        let mut checks = 0;
        let outcome = prepared.publish(|| {
            checks += 1;
            if checks == 2 {
                let replacement = directory.path().join("replacement");
                fs::write(&replacement, b"new user's configuration").unwrap();
                fs::rename(replacement, &path).unwrap();
            }
            true
        });
        assert_eq!(
            outcome,
            Outcome::CreationUncertain(Uncertainty::PublishedFileChanged)
        );
        assert_eq!(fs::read(path).unwrap(), b"new user's configuration");
    }

    #[test]
    fn restrictive_umask_is_confined_to_an_isolated_child() {
        const MARKER: &str = "CAGENTS_CODING_CONFIG_UMASK_CHILD";
        if let Some(path) = std::env::var_os(MARKER) {
            let root = Root::open(std::path::Path::new(&path)).unwrap();
            // SAFETY: this exact child test is the only selected test in its
            // fresh process. No other process/thread's umask is changed.
            unsafe { libc::umask(0o777) };
            let prepared = prepare(&root, false).expect("anonymous staging must qualify");
            assert_eq!(prepared.publish(|| true), Outcome::Created);
            return;
        }
        let (directory, _) = fixture();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "account::coding_config::tests::restrictive_umask_is_confined_to_an_isolated_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(MARKER, directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata = fs::metadata(directory.path().join(NAME)).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(fs::read(directory.path().join(NAME)).unwrap(), OFFLINE);
    }
}
