//! Scoped readers of the exact account home and workspace used by a diagnostic.
//!
//! Capture outside host/map locks, then enroll under the caller's short control
//! guard after account-mutation and worktree-removal admission. Destructive
//! operations use the same guard around their final reader census/reservation.
//! Ordinary launches never consult these leases; readers do not exclude readers.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

pub(in crate::host) type Leases = Arc<Mutex<BTreeMap<Uuid, Arc<Scope>>>>;

struct Directory {
    path: PathBuf,
    identity: (u64, u64),
    ancestors: BTreeSet<(u64, u64)>,
    pin: File,
}

impl Directory {
    fn capture(path: &Path, pin: &File) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("The account check scope must be absolute".into());
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| "The account check scope is unavailable")?;
        let pin = pin
            .try_clone()
            .map_err(|_| "The account check scope cannot be retained")?;
        let metadata = pin
            .metadata()
            .map_err(|_| "The account check scope cannot be identified")?;
        if !metadata.is_dir() {
            return Err("The account check scope is not a directory".into());
        }
        let identity = (metadata.dev(), metadata.ino());
        let ancestors =
            super::super::account_lifecycle::physical_ancestors(&canonical, Some(&pin))?;
        // The supplied descriptor belongs to the caller's already captured
        // operand, never a freshly followed replacement at that spelling.
        for current in [canonical.as_path(), path] {
            let metadata = std::fs::metadata(current)
                .map_err(|_| "The account check scope changed during capture")?;
            if !metadata.is_dir() || (metadata.dev(), metadata.ino()) != identity {
                return Err("The account check scope changed during capture".into());
            }
        }
        Ok(Self {
            path: canonical,
            identity,
            ancestors,
            pin,
        })
    }

    fn overlaps(&self, target: &DestructiveScope) -> Result<bool, String> {
        // Preserve the captured namespace until Drop, and refresh through the
        // descriptor so a rename/reparent cannot hide its current physical use.
        if self.path.starts_with(&target.path)
            || target.path.starts_with(&self.path)
            || target.ancestors.contains(&self.identity)
            || target
                .identity
                .is_some_and(|identity| self.ancestors.contains(&identity))
        {
            return Ok(true);
        }
        let ancestors =
            super::super::account_lifecycle::physical_ancestors(&self.path, Some(&self.pin))?;
        Ok(target
            .identity
            .is_some_and(|identity| ancestors.contains(&identity)))
    }
}

/// Both descriptors remain alive until the lease and any census snapshots drop.
/// This is occupancy evidence, not authority to execute or delete anything.
pub(in crate::host) struct Scope {
    home: Directory,
    workspace: Directory,
}
impl Scope {
    pub(in crate::host) fn capture(
        home: &Path,
        home_pin: &File,
        workspace: &Path,
        workspace_pin: &File,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            home: Directory::capture(home, home_pin)?,
            workspace: Directory::capture(workspace, workspace_pin)?,
        }))
    }
}

/// Removal on Drop also covers provider errors and early-return paths.
pub(in crate::host) struct Lease {
    map: Leases,
    token: Uuid,
}
impl Lease {
    /// Caller has completed overlapping account/removal checks and still holds
    /// the shared control guard that serializes destructive scope enrollment.
    /// No filesystem work, provider work or waiting is performed while locked.
    pub(in crate::host) fn enroll(map: &Leases, scope: Arc<Scope>) -> Self {
        let token = Uuid::new_v4();
        map.lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token, scope);
        Self {
            map: Arc::clone(map),
            token,
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.token);
    }
}

pub(in crate::host) fn active_count(map: &Leases) -> usize {
    map.lock().unwrap_or_else(|p| p.into_inner()).len()
}

/// Refuse deletion/mutation of a directory containing, contained by, or equal
/// to either pinned operand. Caller coordinates the final census with the same
/// short control guard as enrollment; this alone is not a deletion permit.
pub(in crate::host) fn ensure_outside(map: &Leases, path: &Path) -> Result<(), String> {
    let readers: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    if readers.is_empty() {
        return Ok(());
    }
    // Never hold the map/HostState lock while resolving paths or walking pins.
    let target = DestructiveScope::capture(path)?;
    ensure_clear(readers, target)
}

/// Census an already captured destructive operand, even if its old spelling
/// now names an unrelated replacement. The caller retains its original path
/// and pin binding and supplies both; do not re-follow the path for identity.
pub(in crate::host) fn ensure_outside_directory(
    map: &Leases,
    path: &Path,
    pin: &File,
) -> Result<(), String> {
    let readers: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .cloned()
        .collect();
    if readers.is_empty() {
        return Ok(());
    }
    let target = DestructiveScope::from_pin(path, pin)?;
    ensure_clear(readers, target)
}

fn ensure_clear(readers: Vec<Arc<Scope>>, target: DestructiveScope) -> Result<(), String> {
    for reader in readers {
        if reader.home.overlaps(&target)? || reader.workspace.overlaps(&target)? {
            return Err("An account check is using this directory or one inside or around it; wait for the check to finish".into());
        }
    }
    Ok(())
}

struct DestructiveScope {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    ancestors: BTreeSet<(u64, u64)>,
    // The target also cannot be recycled during a physical census.
    _pin: Option<File>,
}
impl DestructiveScope {
    fn from_pin(path: &Path, pin: &File) -> Result<Self, String> {
        validate_path(path)?;
        let pin = pin
            .try_clone()
            .map_err(|_| "The destructive directory scope cannot be retained")?;
        let metadata = pin
            .metadata()
            .map_err(|_| "The destructive directory scope cannot be identified")?;
        if !metadata.is_dir() {
            return Err("The destructive scope is not a directory".into());
        }
        let ancestors = super::super::account_lifecycle::physical_ancestors(path, Some(&pin))?;
        Ok(Self {
            path: path.to_path_buf(),
            identity: Some((metadata.dev(), metadata.ino())),
            ancestors,
            _pin: Some(pin),
        })
    }

    fn capture(path: &Path) -> Result<Self, String> {
        let path = normalize(path)?;
        let pin = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(pin) => Some(pin),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("The destructive directory scope cannot be pinned".into()),
        };
        let identity = pin
            .as_ref()
            .map(|pin| pin.metadata())
            .transpose()
            .map_err(|_| "The destructive directory scope cannot be identified")?
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let ancestors = super::super::account_lifecycle::physical_ancestors(&path, pin.as_ref())?;
        Ok(Self {
            path,
            identity,
            ancestors,
            _pin: pin,
        })
    }
}

// Missing descendants still participate in the namespace guard, without
// interpreting '..' through a missing component or using this client's HOME.
fn normalize(path: &Path) -> Result<PathBuf, String> {
    validate_path(path)?;
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut canonical) => {
                canonical.extend(missing.into_iter().rev());
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .ok_or("The directory scope has no existing parent")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or("The directory scope has no existing parent")?;
            }
            Err(_) => return Err("The destructive directory scope cannot be resolved".into()),
        }
    }
}

fn validate_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(
            "The destructive directory scope must be absolute without parent traversal".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn pin(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .unwrap()
    }
    fn capture(home: &Path, workspace: &Path) -> Arc<Scope> {
        Scope::capture(home, &pin(home), workspace, &pin(workspace)).unwrap()
    }

    #[test]
    fn home_and_workspace_block_both_overlap_directions_but_allow_siblings() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("accounts/selected");
        let workspace = root.path().join("projects/selected");
        let sibling = root.path().join("projects/other");
        for path in [&home, &workspace, &sibling] {
            fs::create_dir_all(path.join("child")).unwrap();
        }
        let map = Leases::default();
        let _lease = Lease::enroll(&map, capture(&home, &workspace));
        for operand in [&home, &workspace] {
            for target in [
                operand.to_path_buf(),
                operand.join("child"),
                operand.join("not-created/child"),
                operand.parent().unwrap().to_path_buf(),
            ] {
                assert!(
                    ensure_outside(&map, &target).is_err(),
                    "overlapping target: {}",
                    target.display()
                );
            }
        }
        assert!(ensure_outside(&map, &sibling).is_ok());
        assert!(ensure_outside(&map, &root.path().join("accounts/another")).is_ok());
        // Sibling and even same-scope readers coexist; this map is not a writer
        // reservation, and therefore cannot block ordinary launches.
        let _sibling = Lease::enroll(&map, capture(&home, &sibling));
        let _same = Lease::enroll(&map, capture(&home, &workspace));
        assert!(ensure_outside(&map, &sibling).is_err());
    }

    #[test]
    fn aliases_and_reparented_roots_cannot_hide_pinned_readers() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let old = root.path().join("old-parent");
        let new = root.path().join("new-parent");
        let workspace = old.join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(workspace.join("child")).unwrap();
        fs::create_dir(&new).unwrap();
        let alias = root.path().join("selected-alias");
        symlink(&workspace, &alias).unwrap();
        let map = Leases::default();
        let lease = Lease::enroll(&map, capture(&home, &alias));
        assert!(ensure_outside(&map, &alias).is_err());
        let moved = new.join("renamed-workspace");
        fs::rename(&workspace, &moved).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&moved, &alias).unwrap();
        for target in [&moved, &moved.join("child"), &new, &alias, &workspace] {
            assert!(
                ensure_outside(&map, target).is_err(),
                "renamed or retained namespace: {}",
                target.display()
            );
        }
        drop(lease);
        assert!(ensure_outside(&map, &new).is_ok());
        assert!(ensure_outside(&map, &old).is_ok());
    }

    #[test]
    fn renamed_ancestor_and_replaced_spelling_keep_the_actual_descendant_occupied() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let parent = root.path().join("parent");
        let workspace = parent.join("workspace");
        fs::create_dir(&home).unwrap();
        fs::create_dir_all(workspace.join("child")).unwrap();
        let map = Leases::default();
        let _lease = Lease::enroll(&map, capture(&home, &workspace));
        let moved = root.path().join("moved-parent");
        fs::rename(&parent, &moved).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        assert!(ensure_outside(&map, &moved).is_err());
        assert!(ensure_outside(&map, &moved.join("workspace/child")).is_err());
        assert!(ensure_outside(&map, &parent).is_err());
        assert!(ensure_outside(&map, &root.path().join("unrelated")).is_ok());
    }

    #[test]
    fn drop_releases_only_its_reader_and_capture_rejects_replaced_pins() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let first = root.path().join("first");
        let second = root.path().join("second");
        for path in [&home, &first, &second] {
            fs::create_dir(path).unwrap();
        }
        let map = Leases::default();
        let first_lease = Lease::enroll(&map, capture(&home, &first));
        let second_lease = Lease::enroll(&map, capture(&home, &second));
        drop(first_lease);
        assert!(ensure_outside(&map, &first).is_ok());
        assert!(ensure_outside(&map, &second).is_err());
        assert!(ensure_outside(&map, &home).is_err());
        drop(second_lease);
        assert!(ensure_outside(&map, &home).is_ok());
        let original = pin(&first);
        fs::rename(&first, root.path().join("retired-first")).unwrap();
        fs::create_dir(&first).unwrap();
        assert!(Scope::capture(&home, &pin(&home), &first, &original).is_err());
    }

    #[test]
    fn destructive_pin_census_follows_the_original_root_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        for path in [&home, &source, &destination] {
            fs::create_dir(path).unwrap();
        }
        let original = source.join("destructive-root");
        fs::create_dir_all(original.join("workspace")).unwrap();
        let destructive_pin = pin(&original);
        let moved = destination.join("renamed-root");
        fs::rename(&original, &moved).unwrap();
        fs::create_dir(&original).unwrap();
        // A reader enrolled after the move does not share the original lexical
        // spelling. Only the destructive operation's retained pin finds it.
        let map = Leases::default();
        let lease = Lease::enroll(&map, capture(&home, &moved.join("workspace")));
        assert!(ensure_outside(&map, &original).is_ok());
        assert!(ensure_outside_directory(&map, &original, &destructive_pin).is_err());
        drop(lease);
        assert!(ensure_outside_directory(&map, &original, &destructive_pin).is_ok());
    }
}
