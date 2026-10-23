//! Metadata-only reverse references to the selected home's fixed config.toml.
//!
//! Roots are already retained by the caller. This is an observation, never an
//! account/lifecycle permit or an atomic filesystem snapshot. The Store caller
//! must census every relevant home under its registry lease both immediately
//! before and after publication, and revalidate the earlier witnesses. External
//! filesystem changes are not serialized by that lease. No file contents are
//! opened, and these private witnesses must not be serialized into a reply.

use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::Metadata,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use crate::fs_safety::{self, Root};

const NAME: &str = "config.toml";
const MAX_STEPS: usize = 128;
const MAX_LINKS: usize = 32;
const MAX_TARGET_BYTES: usize = 16 * 1024;
const MAX_OBSERVATIONS: usize = MAX_STEPS + MAX_LINKS + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Independent,
    Reference,
    Unknown,
}

pub(crate) struct Observation {
    pub(crate) verdict: Verdict,
    /// None means resolution could not be established. Even a successfully
    /// revalidated witness requires a fresh post-publication target census.
    pub(crate) witness: Option<Witness>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
    // Inserting the selected config legitimately changes its directory's
    // size/timestamps. Directory identity, type, ownership and mode still
    // matter; non-directory changes are conservatively treated as unstable.
    details: Option<(u64, i64, i64, i64, i64)>,
}

impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        Self {
            identity: Identity::of(metadata),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            links: metadata.nlink(),
            details: (!metadata.is_dir()).then(|| {
                (
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            }),
        }
    }
}

enum Observed {
    Directory {
        root: Root,
        stamp: Stamp,
    },
    Entry {
        parent: Root,
        name: OsString,
        stamp: Option<Stamp>,
        target: Option<PathBuf>,
    },
    Parent {
        child: Root,
        parent: Identity,
    },
}

/// Opaque, bounded descriptor/metadata evidence for one other fixed config.
/// This records the walked namespace, not the selected file's absence: the
/// caller's intended publication changes that absence between its two passes.
pub(crate) struct Witness {
    observations: Vec<Observed>,
}

impl Witness {
    /// Lets the Store caller bound retained observations across its full census.
    pub(crate) fn observation_count(&self) -> usize {
        self.observations.len()
    }

    fn push(&mut self, observation: Observed) -> Result<(), ()> {
        if self.observations.len() == MAX_OBSERVATIONS {
            return Err(());
        }
        self.observations.push(observation);
        Ok(())
    }

    fn directory(&mut self, root: &Root) -> Result<(), ()> {
        self.push(Observed::Directory {
            root: root.try_clone().map_err(|_| ())?,
            stamp: Stamp::of(&live_directory(root)?),
        })
    }

    /// Rechecks the original retained observations. False is a fixed context
    /// refusal/uncertainty; it is never permission to discard the old witness
    /// and silently adopt a different other-account target.
    pub(crate) fn revalidate(&self) -> bool {
        self.observations
            .iter()
            .all(|observation| match observation {
                Observed::Directory { root, stamp } => {
                    live_directory(root).is_ok_and(|metadata| Stamp::of(&metadata) == *stamp)
                }
                Observed::Entry {
                    parent,
                    name,
                    stamp,
                    target,
                } => {
                    if live_directory(parent).is_err() {
                        return false;
                    }
                    let name = Path::new(name);
                    let Ok(current) = entry(parent, name) else {
                        return false;
                    };
                    if current.as_ref().map(Stamp::of) != *stamp {
                        return false;
                    }
                    target.as_ref().is_none_or(|expected| {
                        fs_safety::read_link_beneath(parent, name)
                            .is_ok_and(|actual| actual == *expected)
                            && entry(parent, name)
                                .is_ok_and(|after| after.as_ref().map(Stamp::of) == *stamp)
                    })
                }
                Observed::Parent { child, parent } => {
                    live_directory(child).is_ok()
                        && fs_safety::retained_parent_directory(child)
                            .ok()
                            .and_then(|root| live_directory(&root).ok())
                            .is_some_and(|metadata| Identity::of(&metadata) == *parent)
                }
            })
    }
}

fn live_directory(root: &Root) -> Result<Metadata, ()> {
    let metadata = root.metadata().map_err(|_| ())?;
    if !metadata.is_dir() || metadata.nlink() == 0 {
        return Err(());
    }
    Ok(metadata)
}

fn entry(root: &Root, name: &Path) -> Result<Option<Metadata>, ()> {
    match fs_safety::metadata_beneath(root, name) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(_) => Err(()),
    }
}

fn selected_entry(root: &Root) -> Result<Option<Stamp>, ()> {
    let metadata = entry(root, Path::new(NAME))?;
    if metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_file())
    {
        return Err(());
    }
    Ok(metadata.as_ref().map(Stamp::of))
}

enum Part {
    Current,
    Parent,
    Name(OsString),
}

// Keep dot components and the trailing-slash directory requirement. Lexical
// Path::components normalization would lose `file/.` and `file/` semantics.
fn target_parts(target: &Path, bytes: &mut usize) -> Result<(bool, VecDeque<Part>), ()> {
    use std::os::unix::ffi::OsStrExt;

    let raw = target.as_os_str().as_bytes();
    *bytes = bytes.checked_add(raw.len()).ok_or(())?;
    if raw.is_empty() || *bytes > MAX_TARGET_BYTES || raw.contains(&0) {
        return Err(());
    }
    let mut parts = VecDeque::new();
    for part in raw
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
    {
        parts.push_back(match part {
            b"." => Part::Current,
            b".." => Part::Parent,
            _ => Part::Name(OsString::from_vec(part.to_vec())),
        });
        if parts.len() > MAX_STEPS {
            return Err(());
        }
    }
    if raw.ends_with(b"/") {
        parts.push_back(Part::Current);
    }
    Ok((raw.starts_with(b"/"), parts))
}

/// Classifies only `other_home/config.toml`, including prospective dangling
/// targets. Neither root is reopened through its original path. Unknown covers
/// inaccessible, unstable or bounded-out chains and missing intermediate
/// components, rather than treating canonicalize(NotFound) as independence.
pub(crate) fn observe(other_home: &Root, selected_home: &Root) -> Observation {
    match resolve(other_home, selected_home) {
        Ok((verdict, witness)) => Observation {
            verdict,
            witness: Some(witness),
        },
        Err(()) => Observation {
            verdict: Verdict::Unknown,
            witness: None,
        },
    }
}

fn resolve(other_home: &Root, selected_home: &Root) -> Result<(Verdict, Witness), ()> {
    let selected_identity = Identity::of(&live_directory(selected_home)?);
    let selected_file = selected_entry(selected_home)?;
    let mut witness = Witness {
        observations: Vec::new(),
    };
    witness.directory(other_home)?;
    witness.directory(selected_home)?;
    let mut directory = other_home.try_clone().map_err(|_| ())?;
    let mut remaining = VecDeque::from([Part::Name(NAME.into())]);
    let mut steps = 0;
    let mut links = 0;
    let mut bytes = NAME.len();
    let verdict = loop {
        steps += 1;
        if steps > MAX_STEPS {
            return Err(());
        }
        let Some(part) = remaining.pop_front() else {
            // A target ending at a known directory cannot name the selected
            // regular/absent config file. It must still remain a live pin.
            witness.directory(&directory)?;
            break Verdict::Independent;
        };
        live_directory(&directory)?;
        match part {
            Part::Current => {}
            Part::Parent => {
                let parent = fs_safety::retained_parent_directory(&directory).map_err(|_| ())?;
                let identity = Identity::of(&live_directory(&parent)?);
                witness.push(Observed::Parent {
                    child: directory,
                    parent: identity,
                })?;
                witness.directory(&parent)?;
                directory = parent;
            }
            Part::Name(name) => {
                let relative = Path::new(&name);
                let metadata = entry(&directory, relative)?;
                let target = if metadata
                    .as_ref()
                    .is_some_and(|m| m.file_type().is_symlink())
                {
                    Some(fs_safety::read_link_beneath(&directory, relative).map_err(|_| ())?)
                } else {
                    None
                };
                witness.push(Observed::Entry {
                    parent: directory.try_clone().map_err(|_| ())?,
                    name: name.clone(),
                    stamp: metadata.as_ref().map(Stamp::of),
                    target: target.clone(),
                })?;
                if let Some(target) = target {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(());
                    }
                    let (absolute, mut expanded) = target_parts(&target, &mut bytes)?;
                    if expanded.len() + remaining.len() > MAX_STEPS {
                        return Err(());
                    }
                    expanded.append(&mut remaining);
                    remaining = expanded;
                    if absolute {
                        directory = Root::open(Path::new("/")).map_err(|_| ())?;
                        witness.directory(&directory)?;
                    }
                    continue;
                }
                if remaining.is_empty() {
                    let parent = Identity::of(&live_directory(&directory)?);
                    let namespace_reference = parent == selected_identity && name == NAME;
                    let inode_reference = metadata.as_ref().is_some_and(|metadata| {
                        selected_file
                            .as_ref()
                            .is_some_and(|selected| selected.identity == Identity::of(metadata))
                    });
                    break if namespace_reference || inode_reference {
                        Verdict::Reference
                    } else {
                        Verdict::Independent
                    };
                }
                // A missing prefix cannot be traversed or normalized through
                // dot/parent components, even if its suffix looks unrelated.
                let metadata = metadata.ok_or(())?;
                if !metadata.is_dir() {
                    return Err(());
                }
                let child =
                    fs_safety::open_directory_beneath(&directory, relative).map_err(|_| ())?;
                if Identity::of(&live_directory(&child)?) != Identity::of(&metadata) {
                    return Err(());
                }
                directory = child;
            }
        }
    };
    if !witness.revalidate() || selected_entry(selected_home)? != selected_file {
        return Err(());
    }
    Ok((verdict, witness))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn homes() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected");
        let other = root.path().join("other");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&other).unwrap();
        (root, selected, other)
    }

    fn inspect(other: &Path, selected: &Path) -> Observation {
        observe(&Root::open(other).unwrap(), &Root::open(selected).unwrap())
    }

    #[test]
    fn distinct_missing_entries_are_independent_without_materialization() {
        let (_root, selected, other) = homes();
        let observed = inspect(&other, &selected);
        assert_eq!(observed.verdict, Verdict::Independent);
        assert!(observed.witness.unwrap().revalidate());
        assert!(!selected.join(NAME).exists());
        assert!(!other.join(NAME).exists());
    }

    #[test]
    fn dangling_absolute_and_relative_links_reference_selected_namespace() {
        for absolute in [false, true] {
            let (_root, selected, other) = homes();
            let target = if absolute {
                selected.join(NAME)
            } else {
                PathBuf::from("../selected/config.toml")
            };
            let link = other.join(NAME);
            symlink(&target, &link).unwrap();
            let original = Stamp::of(&fs::symlink_metadata(&link).unwrap());
            let observed = inspect(&other, &selected);
            assert_eq!(observed.verdict, Verdict::Reference);
            assert!(observed.witness.unwrap().revalidate());
            assert!(original == Stamp::of(&fs::symlink_metadata(&link).unwrap()));
            assert_eq!(fs::read_link(&link).unwrap(), target);
            assert!(!selected.join(NAME).exists());
        }
    }

    #[test]
    fn multiple_links_and_parent_aliases_reach_the_dangling_selected_file() {
        let (root, selected, other) = homes();
        symlink(&selected, root.path().join("selected-alias")).unwrap();
        symlink("../selected-alias/config.toml", other.join("bridge")).unwrap();
        symlink("bridge", other.join(NAME)).unwrap();
        let observed = inspect(&other, &selected);
        assert_eq!(observed.verdict, Verdict::Reference);
        assert!(observed.witness.unwrap().revalidate());
        assert!(!selected.join(NAME).exists());
    }

    #[test]
    fn dot_dot_follows_the_physical_parent_after_a_directory_symlink() {
        let (_root, selected, other) = homes();
        fs::create_dir(selected.join("child")).unwrap();
        symlink(selected.join("child"), other.join("alias")).unwrap();
        // Lexically cancelling alias/.. would incorrectly stay in other.
        symlink("alias/../config.toml", other.join(NAME)).unwrap();
        let observed = inspect(&other, &selected);
        assert_eq!(observed.verdict, Verdict::Reference);
        assert!(observed.witness.unwrap().revalidate());
    }

    #[test]
    fn system_style_shared_links_work_in_both_selected_directions() {
        for select_system in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let system = root.path().join("system");
            let imported = root.path().join("imported");
            fs::create_dir(&system).unwrap();
            fs::create_dir(&imported).unwrap();
            let (selected, other) = if select_system {
                (&system, &imported)
            } else {
                (&imported, &system)
            };
            symlink(selected.join(NAME), other.join(NAME)).unwrap();
            assert_eq!(inspect(other, selected).verdict, Verdict::Reference);
        }
    }

    #[test]
    fn regular_file_metadata_is_independent_but_an_existing_hardlink_is_a_reference() {
        let (_root, selected, other) = homes();
        fs::write(
            other.join(NAME),
            b"private fixture bytes are never read by resolver",
        )
        .unwrap();
        let observed = inspect(&other, &selected);
        assert_eq!(observed.verdict, Verdict::Independent);
        let witness = observed.witness.unwrap();
        assert!(witness.revalidate());
        fs::hard_link(other.join(NAME), selected.join(NAME)).unwrap();
        assert!(
            !witness.revalidate(),
            "the original file's link count changed"
        );
        assert_eq!(inspect(&other, &selected).verdict, Verdict::Reference);
    }

    #[test]
    fn unrelated_dangling_leaf_is_preserved_and_selected_publication_is_not_drift() {
        let (root, selected, other) = homes();
        let unrelated = root.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        symlink("../unrelated/missing.toml", other.join(NAME)).unwrap();
        let original = Stamp::of(&fs::symlink_metadata(other.join(NAME)).unwrap());
        let observed = inspect(&other, &selected);
        assert_eq!(observed.verdict, Verdict::Independent);
        let witness = observed.witness.unwrap();
        fs::write(selected.join(NAME), b"owned publication fixture").unwrap();
        assert!(witness.revalidate());
        assert_eq!(inspect(&other, &selected).verdict, Verdict::Independent);
        assert!(original == Stamp::of(&fs::symlink_metadata(other.join(NAME)).unwrap()));
        assert_eq!(
            fs::read_link(other.join(NAME)).unwrap(),
            Path::new("../unrelated/missing.toml")
        );
        assert!(!unrelated.join("missing.toml").exists());
    }

    #[test]
    fn loops_missing_intermediate_and_file_directory_suffixes_are_unknown() {
        for target in [
            "loop",
            "missing/../selected/config.toml",
            "regular/.",
            "regular/",
        ] {
            let (_root, selected, other) = homes();
            symlink(NAME, other.join("loop")).unwrap();
            fs::write(other.join("regular"), b"metadata only").unwrap();
            symlink(target, other.join(NAME)).unwrap();
            let observed = inspect(&other, &selected);
            assert_eq!(observed.verdict, Verdict::Unknown, "{target}");
            assert!(observed.witness.is_none());
            assert!(!selected.join(NAME).exists());
        }
    }

    #[test]
    fn witnesses_detect_retarget_missing_leaf_creation_and_physical_reparenting() {
        let (root, selected, other) = homes();
        let unrelated = root.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        symlink("../unrelated/missing.toml", other.join(NAME)).unwrap();
        let before_retarget = inspect(&other, &selected).witness.unwrap();
        fs::remove_file(other.join(NAME)).unwrap();
        symlink(selected.join(NAME), other.join(NAME)).unwrap();
        assert!(!before_retarget.revalidate());
        assert_eq!(inspect(&other, &selected).verdict, Verdict::Reference);
        fs::remove_file(other.join(NAME)).unwrap();
        symlink("../unrelated/missing.toml", other.join(NAME)).unwrap();
        let before_create = inspect(&other, &selected).witness.unwrap();
        fs::write(unrelated.join("missing.toml"), b"new fixture entry").unwrap();
        assert!(!before_create.revalidate());
        let moved_parent = root.path().join("moved-parent");
        fs::create_dir(&moved_parent).unwrap();
        let before_reparent = inspect(&other, &selected).witness.unwrap();
        fs::rename(&other, moved_parent.join("other")).unwrap();
        assert!(!before_reparent.revalidate());
    }

    #[test]
    fn inaccessible_target_parent_is_unknown_without_changing_it() {
        use std::os::unix::fs::PermissionsExt;

        let (root, selected, other) = homes();
        let inaccessible = root.path().join("inaccessible");
        fs::create_dir(&inaccessible).unwrap();
        symlink("../inaccessible/missing.toml", other.join(NAME)).unwrap();
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
        let observed = inspect(&other, &selected);
        // Restore only this owned fixture before any assertion/TempDir cleanup.
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(observed.verdict, Verdict::Unknown);
        assert!(observed.witness.is_none());
    }
}
