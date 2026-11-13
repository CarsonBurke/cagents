//! Private test directories whose ancestors cannot discover a developer's Git
//! checkout or provider configuration. This module never changes the process
//! environment or repairs an unsafe temporary directory.
use std::{
    ffi::{CStr, OsStr},
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
};

pub(crate) fn tempdir() -> io::Result<tempfile::TempDir> {
    tempdir_with_prefix("cagents-test-")
}

pub(crate) fn tempdir_with_prefix(prefix: &str) -> io::Result<tempfile::TempDir> {
    try_tempdir_in(&std::env::temp_dir(), prefix)
}

/// Explicit roots use the same protections as TMPDIR. In particular, changing
/// HOME does not make the account's real home an acceptable fixture location.
pub(crate) fn try_tempdir_in(root: &Path, prefix: &str) -> io::Result<tempfile::TempDir> {
    create_in(root, prefix, &protected_roots()?)
}

fn refusal(root: &Path, reason: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "unsafe test fixture root {}: {reason}; set TMPDIR to an existing writable directory outside account HOME, inherited HOME/CODEX_HOME, and Git checkouts, with no symlink components",
        root.display()
    ))
}

fn create_in(
    root: &Path,
    prefix: &str,
    protected: &[(PathBuf, &'static str)],
) -> io::Result<tempfile::TempDir> {
    validate_root(root, protected)?;
    if prefix.chars().any(|part| matches!(part, '/' | '\\' | '\0')) {
        return Err(refusal(root, "fixture prefix must be a filename prefix"));
    }
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(root)
        .map_err(|error| refusal(root, format!("cannot create a private directory: {error}")))?;
    let metadata = fs::symlink_metadata(directory.path())
        .map_err(|error| refusal(root, format!("cannot verify private directory: {error}")))?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(refusal(
            root,
            "new directory must be owned by the effective user with mode0700; check the process umask",
        ));
    }
    Ok(directory)
}

fn validate_root(root: &Path, protected: &[(PathBuf, &'static str)]) -> io::Result<()> {
    if !root.is_absolute() || root.components().any(|part| part == Component::ParentDir) {
        return Err(refusal(
            root,
            "the directory must be absolute without '..' components",
        ));
    }
    for (home, source) in protected {
        if root.starts_with(home) {
            return Err(refusal(root, format!("the directory is inside {source}")));
        }
    }
    // Inspect the original path one component at a time. Canonicalizing first
    // would silently accept a TMPDIR symlink into another location.
    let mut ancestor = PathBuf::new();
    for component in root.components() {
        ancestor.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&ancestor).map_err(|error| {
            refusal(
                root,
                format!("cannot inspect {}: {error}", ancestor.display()),
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(refusal(
                root,
                format!(
                    "{} must be a directory, not a symlink or file",
                    ancestor.display()
                ),
            ));
        }
        let git = ancestor.join(".git");
        match fs::symlink_metadata(&git) {
            Ok(_) => {
                return Err(refusal(
                    root,
                    format!("enclosing Git marker {} exists", git.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(refusal(
                    root,
                    format!("cannot inspect {}: {error}", git.display()),
                ));
            }
        }
    }
    Ok(())
}

fn protected_roots() -> io::Result<Vec<(PathBuf, &'static str)>> {
    let mut roots = Vec::new();
    // SAFETY: these identity queries take no pointer arguments.
    let (real, effective) = unsafe { (libc::getuid(), libc::geteuid()) };
    roots.push((resolve_protected(&account_home(real)?)?, "the account home"));
    if effective != real {
        roots.push((
            resolve_protected(&account_home(effective)?)?,
            "the effective account home",
        ));
    }
    for variable in ["HOME", "CODEX_HOME"] {
        if let Some(value) = std::env::var_os(variable) {
            roots.push((resolve_protected(Path::new(&value))?, variable));
        }
    }
    Ok(roots)
}

/// Resolve even a not-yet-created CODEX_HOME through its existing ancestors so
/// an inherited symlink cannot hide a protected location from the root check.
fn resolve_protected(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(refusal(path, "cannot establish an absolute protected home"));
    }
    // Resolve the original components, including symlink/.. semantics. Lexical
    // normalization first could protect a different directory from HOME.
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for name in missing.into_iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A dangling symlink is an existing protected location with an
                // unknown target, not a missing path component to reconstruct.
                match fs::symlink_metadata(existing) {
                    Ok(_) => {
                        return Err(refusal(path, "cannot resolve existing protected home"));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(refusal(path, error)),
                }
                let name = existing
                    .file_name()
                    .ok_or_else(|| refusal(path, "cannot resolve protected home"))?;
                missing.push(name);
                existing = existing
                    .parent()
                    .ok_or_else(|| refusal(path, "cannot resolve protected home"))?;
            }
            Err(error) => {
                return Err(refusal(
                    path,
                    format!("cannot resolve protected home: {error}"),
                ));
            }
        }
    }
}

fn account_home(uid: libc::uid_t) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: the entry, result pointer and buffer are live writable storage.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return Err(refusal(
                Path::new("<account home>"),
                format!("cannot look up account home for UID {uid} (status {status})"),
            ));
        }
        // SAFETY: successful getpwuid_r initialized the entry; its strings are
        // NUL-terminated and remain in the live buffer until this function exits.
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return Err(refusal(
                Path::new("<account home>"),
                "account home is unavailable",
            ));
        }
        let bytes = unsafe { CStr::from_ptr(directory) }.to_bytes();
        let path = Path::new(OsStr::from_bytes(bytes));
        if !path.is_absolute() {
            return Err(refusal(path, "account home is not absolute"));
        }
        return Ok(path.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // No fallback to a hardcoded /tmp or /var/tmp: the outer test process must
    // select a safe TMPDIR before startup, just as the migrated fixtures do.
    fn fixture() -> tempfile::TempDir {
        tempdir_with_prefix("cagents-fixture-policy-").unwrap()
    }

    #[test]
    fn creates_distinct_owned_private_leaves() {
        let fixture = fixture();
        let first = try_tempdir_in(fixture.path(), "first-").unwrap();
        let second = try_tempdir_in(fixture.path(), "first-").unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(first.path().parent(), Some(fixture.path()));
        let metadata = fs::symlink_metadata(first.path()).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }

    #[test]
    fn refuses_relative_parent_and_file_roots() {
        let fixture = fixture();
        let file = fixture.path().join("file");
        fs::write(&file, b"owned sentinel").unwrap();
        for root in [
            PathBuf::from("relative"),
            fixture.path().join("child/.."),
            fixture.path().join("missing"),
            file.clone(),
        ] {
            let error = create_in(&root, "case-", &[]).unwrap_err().to_string();
            assert!(error.contains("set TMPDIR"), "{error}");
            assert!(error.contains(&root.display().to_string()), "{error}");
        }
        assert_eq!(fs::read(file).unwrap(), b"owned sentinel");
    }

    #[test]
    fn refuses_symlink_root_and_ancestor() {
        let fixture = fixture();
        let actual = fixture.path().join("actual");
        fs::create_dir_all(actual.join("nested")).unwrap();
        let link = fixture.path().join("link");
        symlink(&actual, &link).unwrap();
        for root in [link.clone(), link.join("nested")] {
            assert!(
                create_in(&root, "case-", &[])
                    .unwrap_err()
                    .to_string()
                    .contains("symlink")
            );
        }
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_dir(actual.join("nested")).unwrap().count(), 0);
    }

    #[test]
    fn refuses_enclosing_git_directory_file_and_dangling_link() {
        let fixture = fixture();
        for kind in ["directory", "file", "dangling-link"] {
            let parent = fixture.path().join(kind);
            let root = parent.join("nested");
            fs::create_dir_all(&root).unwrap();
            let marker = parent.join(".git");
            match kind {
                "directory" => fs::create_dir(&marker).unwrap(),
                "file" => fs::write(&marker, b"gitdir: owned sentinel").unwrap(),
                _ => symlink("owned-missing-target", &marker).unwrap(),
            }
            let error = create_in(&root, "case-", &[]).unwrap_err().to_string();
            assert!(error.contains("enclosing Git marker"), "{error}");
            assert!(fs::symlink_metadata(marker).is_ok());
            assert_eq!(fs::read_dir(root).unwrap().count(), 0);
        }
    }

    #[test]
    fn refuses_account_and_inherited_home_roots() {
        let fixture = fixture();
        for source in ["the account home", "HOME", "CODEX_HOME"] {
            let home = fixture.path().join(source.replace(' ', "-"));
            fs::create_dir_all(home.join("nested")).unwrap();
            let protected = vec![(home.clone(), source)];
            for root in [&home, &home.join("nested")] {
                let error = create_in(root, "case-", &protected)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(source), "{error}");
            }
            assert_eq!(fs::read_dir(home.join("nested")).unwrap().count(), 0);
        }
    }

    #[test]
    fn allows_siblings_of_protected_homes() {
        let fixture = fixture();
        let home = fixture.path().join("home");
        let sibling = fixture.path().join("home-fixture");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&sibling).unwrap();
        let leaf = create_in(&sibling, "case-", &[(home, "HOME")]).unwrap();
        assert_eq!(leaf.path().parent(), Some(sibling.as_path()));
    }

    #[test]
    fn resolves_missing_protected_home_through_symlink_ancestor() {
        let fixture = fixture();
        let actual = fixture.path().join("actual");
        fs::create_dir(&actual).unwrap();
        let link = fixture.path().join("link");
        symlink(&actual, &link).unwrap();
        let protected = resolve_protected(&link.join("missing/home")).unwrap();
        assert_eq!(protected, actual.join("missing/home"));
        fs::create_dir_all(&protected).unwrap();
        assert!(create_in(&protected, "case-", &[(protected.clone(), "HOME")]).is_err());
    }

    #[test]
    fn resolves_parent_after_symlink_before_protecting_home() {
        let fixture = fixture();
        let destination = fixture.path().join("destination");
        fs::create_dir_all(destination.join("child")).unwrap();
        let link = fixture.path().join("link");
        symlink(destination.join("child"), &link).unwrap();
        let protected = resolve_protected(&link.join("../missing-home")).unwrap();
        assert_eq!(protected, destination.join("missing-home"));
    }

    #[test]
    fn refuses_dangling_protected_home() {
        let fixture = fixture();
        let link = fixture.path().join("dangling-home");
        symlink("owned-missing-home", &link).unwrap();
        assert!(resolve_protected(&link).is_err());
        assert!(resolve_protected(&link.join("missing")).is_err());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn refuses_unresolved_relative_home_and_escaping_prefix() {
        assert!(resolve_protected(Path::new("relative-home")).is_err());
        let fixture = fixture();
        for prefix in ["../escape", "nested/name", "bad\0name", "nested\\name"] {
            assert!(create_in(fixture.path(), prefix, &[]).is_err());
        }
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    fn preserves_inherited_home_settings() {
        let before = [std::env::var_os("HOME"), std::env::var_os("CODEX_HOME")];
        let fixture = tempdir().unwrap();
        let after = [std::env::var_os("HOME"), std::env::var_os("CODEX_HOME")];
        assert_eq!(before, after);
        for (home, _) in protected_roots().unwrap() {
            assert!(!fixture.path().starts_with(home));
        }
    }

    #[test]
    #[ignore = "requires externally selected unsafe TMPDIR with real HOME/CODEX_HOME preserved; run explicitly with --ignored"]
    fn refuses_selected_unsafe_root_before_creating_fixture() {
        let root = std::env::temp_dir();
        let protected = protected_roots().expect("preserve valid inherited account homes");
        // Refuse a mistaken safe-root invocation before the API could create a
        // directory. This case needs no fixture bootstrap or environment edits.
        assert!(
            validate_root(&root, &protected).is_err(),
            "select an unsafe TMPDIR externally for this ignored negative case"
        );
        let error = tempdir()
            .expect_err("unsafe selected TMPDIR must refuse fixture creation")
            .to_string();
        assert!(error.contains("unsafe test fixture root"), "{error}");
        assert!(error.contains("set TMPDIR"), "{error}");
        assert!(error.contains(&root.display().to_string()), "{error}");
    }
}
