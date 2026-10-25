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
const MAX_LEASES: usize = 256;

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
    retained: Mutex<Retained>,
    // Import verification also retains its account reservation and probe lock.
    // The scoped map keeps these alive until original child cleanup is known.
    _guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
}
#[derive(Default)]
struct Retained {
    active_leases: usize,
    children: Vec<UnconfirmedChild>,
}
struct UnconfirmedChild {
    child: tokio::process::Child,
    group_signal_confirmed: bool,
}
impl Scope {
    pub(in crate::host) fn capture(
        home: &Path,
        home_pin: &File,
        workspace: &Path,
        workspace_pin: &File,
    ) -> Result<Arc<Self>, String> {
        Self::capture_inner(home, home_pin, workspace, workspace_pin, None)
    }

    pub(in crate::host) fn capture_guarded(
        home: &Path,
        home_pin: &File,
        workspace: &Path,
        workspace_pin: &File,
        guard: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<Arc<Self>, String> {
        Self::capture_inner(home, home_pin, workspace, workspace_pin, Some(guard))
    }

    fn capture_inner(
        home: &Path,
        home_pin: &File,
        workspace: &Path,
        workspace_pin: &File,
        guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            home: Directory::capture(home, home_pin)?,
            workspace: Directory::capture(workspace, workspace_pin)?,
            retained: Mutex::new(Retained::default()),
            _guard: guard,
        }))
    }
}

/// Drop releases a confirmed scope. Unconfirmed original children keep its map
/// token occupied until a later nonblocking census proves they were reaped.
pub(in crate::host) struct Lease {
    map: Leases,
    token: Uuid,
    scope: Arc<Scope>,
}
impl Lease {
    /// Caller has completed overlapping account/removal checks and still holds
    /// the shared control guard that serializes destructive scope enrollment.
    /// No filesystem work, provider work or waiting is performed while locked.
    pub(in crate::host) fn enroll(map: &Leases, scope: Arc<Scope>) -> Result<Self, String> {
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        if entries.len() >= MAX_LEASES {
            return Err(
                "Too many account checks are active or awaiting cleanup; wait and retry".into(),
            );
        }
        let token = Uuid::new_v4();
        scope
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_leases += 1;
        entries.insert(token, Arc::clone(&scope));
        drop(entries);
        Ok(Self {
            map: Arc::clone(map),
            token,
            scope,
        })
    }

    /// Transfer custody of the original handle when its owner cannot prove
    /// exit. Never reconstruct a handle from a PID or signal a discovered PID.
    pub(in crate::host) fn retain_unconfirmed(
        &self,
        child: tokio::process::Child,
        group_signal_confirmed: bool,
    ) {
        self.scope
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .children
            .push(UnconfirmedChild {
                child,
                group_signal_confirmed,
            });
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // All paths taking both locks use map -> retained. try_wait is never
        // called with the map lock held.
        let mut entries = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let mut retained = self
            .scope
            .retained
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        retained.active_leases -= 1;
        if retained.active_leases == 0 && retained.children.is_empty() {
            entries.remove(&self.token);
        }
    }
}

/// Poll only the retained original handles. None/errors preserve uncertainty;
/// only a confirmed group signal followed by a successful reap can release
/// their pinned directories. A failed/unconfirmed group signal is quarantined
/// even if its leader exits; leader exit alone cannot establish group cleanup. No
/// waiting, PID lookup, signals, timers or background cleanup task is involved.
pub(in crate::host) fn cleanup_retained(map: &Leases) {
    let readers: Vec<_> = map
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(token, scope)| (*token, Arc::clone(scope)))
        .collect();
    for (token, scope) in readers {
        {
            let mut retained = scope.retained.lock().unwrap_or_else(|p| p.into_inner());
            retained.children.retain_mut(|record| {
                !record.group_signal_confirmed || !matches!(record.child.try_wait(), Ok(Some(_)))
            });
        }
        // Enrollment can race this poll, so recheck under the same lock order
        // before retiring this exact snapshot token.
        let mut entries = map.lock().unwrap_or_else(|p| p.into_inner());
        let retained = scope.retained.lock().unwrap_or_else(|p| p.into_inner());
        if retained.active_leases == 0
            && retained.children.is_empty()
            && entries
                .get(&token)
                .is_some_and(|current| Arc::ptr_eq(current, &scope))
        {
            entries.remove(&token);
        }
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
        let _lease = Lease::enroll(&map, capture(&home, &workspace)).unwrap();
        cleanup_retained(&map);
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
        let _sibling = Lease::enroll(&map, capture(&home, &sibling)).unwrap();
        let _same = Lease::enroll(&map, capture(&home, &workspace)).unwrap();
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
        let lease = Lease::enroll(&map, capture(&home, &alias)).unwrap();
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
        let _lease = Lease::enroll(&map, capture(&home, &workspace)).unwrap();
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
        let first_lease = Lease::enroll(&map, capture(&home, &first)).unwrap();
        let second_lease = Lease::enroll(&map, capture(&home, &second)).unwrap();
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
        let lease = Lease::enroll(&map, capture(&home, &moved.join("workspace"))).unwrap();
        assert!(ensure_outside(&map, &original).is_ok());
        assert!(ensure_outside_directory(&map, &original, &destructive_pin).is_err());
        drop(lease);
        assert!(ensure_outside_directory(&map, &original, &destructive_pin).is_ok());
    }

    #[test]
    fn concurrent_enrollment_respects_the_total_bound_and_reopens_after_release() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&workspace).unwrap();
        let scope = capture(&home, &workspace);
        let map = Leases::default();
        let leases: Vec<_> = (0..MAX_LEASES - 1)
            .map(|_| Lease::enroll(&map, Arc::clone(&scope)).unwrap())
            .collect();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let results = std::thread::scope(|threads| {
            let attempts: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let map = Arc::clone(&map);
                    let scope = Arc::clone(&scope);
                    threads.spawn(move || {
                        barrier.wait();
                        Lease::enroll(&map, scope)
                    })
                })
                .collect();
            barrier.wait();
            attempts
                .into_iter()
                .map(|attempt| attempt.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(Lease::enroll(&map, Arc::clone(&scope)).is_err());
        drop(results);
        drop(leases);
        cleanup_retained(&map);
        assert!(ensure_outside(&map, &workspace).is_ok());
        let _next = Lease::enroll(&map, scope).unwrap();
        assert!(ensure_outside(&map, &workspace).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imported_probe_guard_survives_cancellation_until_original_child_is_reaped() {
        use std::process::Stdio;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let guard = Arc::new(());
        let observed = Arc::downgrade(&guard);
        let scope =
            Scope::capture_guarded(&home, &pin(&home), &home, &pin(&home), guard.clone()).unwrap();
        let map = Leases::default();
        let lease = Lease::enroll(&map, scope).unwrap();
        drop(guard);
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        // As in the existing finite original-child fixture, stdin EOF is the
        // sole completion path; no discovered PID or external process is used.
        lease.retain_unconfirmed(child, true);
        drop(lease);
        cleanup_retained(&map);
        assert!(
            observed.upgrade().is_some(),
            "probe serialization and reservation must outlive their cancelled waiter"
        );
        assert_eq!(active_count(&map), 1);
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                cleanup_retained(&map);
                if active_count(&map) == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the original child must be reaped after its finite EOF");
        assert!(
            observed.upgrade().is_none(),
            "confirmed retirement releases the owned guard"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn original_unreaped_child_keeps_its_scope_after_the_lease_drops() {
        use std::process::Stdio;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let sibling = root.path().join("sibling");
        for path in [&home, &workspace, &sibling] {
            fs::create_dir(path).unwrap();
        }
        let map = Leases::default();
        let lease = Lease::enroll(&map, capture(&home, &workspace)).unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(&workspace)
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        lease.retain_unconfirmed(child, true);
        drop(lease);
        cleanup_retained(&map);
        assert!(ensure_outside(&map, &home).is_err());
        assert!(ensure_outside(&map, &workspace).is_err());
        assert!(ensure_outside(&map, &sibling).is_ok());
        // Only this original child's own stdin EOF completes the finite fixture.
        // The census never looks up a PID or signals any process.
        drop(input);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                cleanup_retained(&map);
                if ensure_outside(&map, &workspace).is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the original child must be reaped after stdin EOF");
        assert!(ensure_outside(&map, &home).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unconfirmed_group_signal_stays_quarantined_after_leader_exit() {
        use std::process::Stdio;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let sibling = root.path().join("sibling");
        for path in [&home, &workspace, &sibling] {
            fs::create_dir(path).unwrap();
        }
        let map = Leases::default();
        let lease = Lease::enroll(&map, capture(&home, &workspace)).unwrap();
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(&workspace)
            .process_group(0)
            .spawn()
            .unwrap();
        assert!(child.wait().await.unwrap().success());
        // The original leader has definitively exited. An unconfirmed group
        // signal still prevents claiming that the whole group was cleaned up.
        lease.retain_unconfirmed(child, false);
        drop(lease);
        for _ in 0..2 {
            cleanup_retained(&map);
            assert!(ensure_outside(&map, &home).is_err());
            assert!(ensure_outside(&map, &workspace).is_err());
            assert!(ensure_outside(&map, &sibling).is_ok());
        }
    }
}
