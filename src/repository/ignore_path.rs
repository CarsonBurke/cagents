//! Literal anchored .gitignore entries. Never interpret a changed path as a
//! user-authored glob: agent-created names may contain glob metacharacters.

use super::RepoPath;

pub(super) fn pattern(path: &RepoPath) -> Result<Vec<u8>, String> {
    let mut pattern = vec![b'/'];
    for byte in path.as_path().as_os_str().as_encoded_bytes() {
        if matches!(*byte, b'\n' | b'\r') {
            return Err("Git ignore rules cannot represent a literal path containing a newline or carriage return.".into());
        }
        if matches!(
            *byte,
            b'\\' | b'!' | b'#' | b'[' | b']' | b'*' | b'?' | b' '
        ) {
            pattern.push(b'\\');
        }
        pattern.push(*byte);
    }
    Ok(pattern)
}

pub(super) fn add(root: &crate::fs_safety::Root, path: &RepoPath) -> Result<(), String> {
    use crate::fs_safety;
    use std::{io::Write, os::unix::fs::MetadataExt, path::Path};
    let rule = pattern(path)?;
    let name = Path::new(".gitignore");
    let original = match fs_safety::open_beneath(root, name) {
        Ok(mut file) => {
            let metadata = fs_safety::metadata_of(&file).map_err(|error| error.to_string())?;
            if !metadata.is_file() {
                return Err(".gitignore is not a regular file".into());
            }
            let bytes = fs_safety::read_bounded(&mut file, 1024 * 1024)
                .map_err(|error| error.to_string())?;
            if bytes.len() > 1024 * 1024 {
                return Err(".gitignore is too large to update safely".into());
            }
            Some((file, metadata, bytes))
        }
        Err(error) if error.is_not_found() => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = original
        .as_ref()
        .map_or_else(Vec::new, |(_, _, bytes)| bytes.clone());
    if bytes
        .split(|byte| *byte == b'\n')
        .any(|line| line.strip_suffix(b"\r").unwrap_or(line) == rule)
    {
        return Ok(());
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(&rule);
    bytes.push(b'\n');
    let (mut temporary, temporary_name) =
        fs_safety::create_temporary_in(root, if original.is_some() { 0o600 } else { 0o644 })
            .map_err(|error| error.to_string())?;
    let result = (|| {
        if let Some((file, _, _)) = &original {
            fs_safety::copy_ownership(file, &temporary).map_err(|error| error.to_string())?;
            fs_safety::copy_access_acl(file, &temporary).map_err(|error| error.to_string())?;
        }
        temporary
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        if let Some((file, metadata, _)) = &original {
            temporary
                .set_permissions(metadata.permissions())
                .map_err(|error| error.to_string())?;
            fs_safety::copy_access_acl(file, &temporary).map_err(|error| error.to_string())?;
            if !fs_safety::copy_extended_attributes(file, &temporary).is_empty() {
                return Err("Could not preserve .gitignore extended attributes".into());
            }
        }
        temporary.sync_all().map_err(|error| error.to_string())?;
        if let Some((_, metadata, original_bytes)) = &original {
            let mut current =
                fs_safety::open_beneath(root, name).map_err(|error| error.to_string())?;
            let current_metadata =
                fs_safety::metadata_of(&current).map_err(|error| error.to_string())?;
            if (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ) != (
                current_metadata.dev(),
                current_metadata.ino(),
                current_metadata.ctime(),
                current_metadata.ctime_nsec(),
            ) || fs_safety::read_bounded(&mut current, 1024 * 1024)
                .map_err(|error| error.to_string())?
                != *original_bytes
            {
                return Err(
                    ".gitignore changed while adding this rule; retry with the current file".into(),
                );
            }
            fs_safety::rename_within(root, &temporary_name, name.as_os_str())
                .map_err(|error| error.to_string())?;
        } else {
            fs_safety::rename_new_within(root, &temporary_name, name.as_os_str())
                .map_err(|error| error.to_string())?;
        }
        fs_safety::sync_directory(root).map_err(|error| {
            format!("Ignore rule was written, but its directory could not be synced: {error}")
        })
    })();
    if result.is_err() {
        let _ = fs_safety::unlink_within(root, &temporary_name);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    fn ignore_rule_updates_the_retained_directory_after_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let worktree = directory.path().join("worktree");
        let original = directory.path().join("original");
        fs::create_dir(&worktree).unwrap();
        fs::write(worktree.join(".gitignore"), b"original-rule\n").unwrap();
        let root = crate::fs_safety::Root::open(&worktree).unwrap();
        fs::rename(&worktree, &original).unwrap();
        fs::create_dir(&worktree).unwrap();
        fs::write(worktree.join(".gitignore"), b"replacement-rule\n").unwrap();
        add(&root, &RepoPath::parse("scratch [draft].txt").unwrap()).unwrap();
        assert_eq!(
            fs::read(original.join(".gitignore")).unwrap(),
            b"original-rule\n/scratch\\ \\[draft\\].txt\n"
        );
        assert_eq!(
            fs::read(worktree.join(".gitignore")).unwrap(),
            b"replacement-rule\n"
        );
    }

    #[test]
    fn special_names_are_ignored_literally_without_hiding_neighboring_paths() {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            for (name, _) in std::env::vars_os() {
                if name.to_string_lossy().starts_with("GIT_") {
                    command.env_remove(name);
                }
            }
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.templateDir=",
                ])
                .args(args)
                .current_dir(directory.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let path = RepoPath::parse("a [literal]*?.txt").unwrap();
        let mut rule = pattern(&path).unwrap();
        rule.push(b'\n');
        fs::write(directory.path().join(".gitignore"), rule).unwrap();
        fs::write(directory.path().join(path.as_path()), b"ignored").unwrap();
        fs::write(directory.path().join("a lZZ.txt"), b"visible").unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(
            directory.path().join("nested/a [literal]*?.txt"),
            b"visible",
        )
        .unwrap();
        assert!(
            git(&["check-ignore", "--", "a [literal]*?.txt"])
                .status
                .success()
        );
        assert_eq!(
            git(&["check-ignore", "--", "a lZZ.txt"]).status.code(),
            Some(1)
        );
        assert_eq!(
            git(&["check-ignore", "--", "nested/a [literal]*?.txt"])
                .status
                .code(),
            Some(1)
        );
    }

    #[test]
    fn adding_a_rule_is_atomic_idempotent_and_preserves_the_existing_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let ignore = directory.path().join(".gitignore");
        fs::write(&ignore, b"# existing rule\nprivate/").unwrap();
        fs::set_permissions(&ignore, fs::Permissions::from_mode(0o640)).unwrap();
        let path = RepoPath::parse("new [literal].txt").unwrap();
        add(
            &crate::fs_safety::Root::open(directory.path()).unwrap(),
            &path,
        )
        .unwrap();
        assert_eq!(
            fs::read(&ignore).unwrap(),
            b"# existing rule\nprivate/\n/new\\ \\[literal\\].txt\n"
        );
        assert_eq!(fs::metadata(&ignore).unwrap().mode() & 0o777, 0o640);
        let before = fs::read(&ignore).unwrap();
        add(
            &crate::fs_safety::Root::open(directory.path()).unwrap(),
            &path,
        )
        .unwrap();
        assert_eq!(fs::read(&ignore).unwrap(), before);
    }

    #[test]
    fn ignore_updates_do_not_follow_symlinks_or_write_through_hardlinks() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("rules");
        fs::write(&victim, b"keep\n").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let ignore = directory.path().join(".gitignore");
        let path = RepoPath::parse("new.txt").unwrap();
        symlink(&victim, &ignore).unwrap();
        assert!(
            add(
                &crate::fs_safety::Root::open(directory.path()).unwrap(),
                &path
            )
            .is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep\n");
        fs::remove_file(&ignore).unwrap();
        fs::hard_link(&victim, &ignore).unwrap();
        add(
            &crate::fs_safety::Root::open(directory.path()).unwrap(),
            &path,
        )
        .unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep\n");
        assert_eq!(fs::read(&ignore).unwrap(), b"keep\n/new.txt\n");
    }

    #[test]
    fn byte_paths_are_preserved_and_unrepresentable_names_are_refused() {
        let path = RepoPath::from_bytes(b"dir/\xff#name ").unwrap();
        assert_eq!(pattern(&path).unwrap(), b"/dir/\xff\\#name\\ ");
        assert!(pattern(&RepoPath::parse("two\nlines").unwrap()).is_err());
        assert!(pattern(&RepoPath::parse("carriage\rreturn").unwrap()).is_err());
    }
}
