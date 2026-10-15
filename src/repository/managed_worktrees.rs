//! Clone-local ignore policy for managed worktree directories.
use super::*;

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Ignore an in-checkout managed root without editing tracked .gitignore.
    /// External roots need no exclusion; ignore entries are anchored and escaped.
    pub fn ensure_managed_directory_ignored(&self, repository: &Path, managed_root: &Path, cancellation: &CancellationToken) -> Result<(), RepositoryError> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        let records = self.list_worktrees(repository, cancellation)?;
        let primary = records.first().ok_or_else(|| ValidationError::Io("Repository has no primary worktree".into()))?;
        let primary = primary.path.canonicalize().map_err(|error| ValidationError::Io(error.to_string()))?;
        let managed = canonical_managed_directory(managed_root).map_err(|error| ValidationError::Io(error.to_string()))?;
        let Ok(relative) = managed.strip_prefix(&primary) else { return Ok(()); };
        if relative.as_os_str().is_empty() {
            return Err(ValidationError::Io("The repository root cannot be its managed worktree directory".into()).into());
        }
        let check = self.git_read(&primary, [OsString::from("check-ignore"), "--quiet".into(), "--no-index".into(), "--".into(), relative.join(".cagents-ignore-check").into_os_string()]);
        let output = self.run(&check, cancellation)?;
        if output.success { return Ok(()); }
        if output.exit_code != Some(1) { return Err(git_failure(&check, output).into()); }
        let common = self.common_dir(repository, cancellation)?;
        let info = common.join("info");
        crate::secure_file::reject_symlink_components(&info).map_err(|error| ValidationError::Io(error.to_string()))?;
        fs::create_dir_all(&info).map_err(|error| ValidationError::Io(error.to_string()))?;
        let exclude = info.join("exclude");
        crate::secure_file::reject_symlink_components(&exclude).map_err(|error| ValidationError::Io(error.to_string()))?;
        let mut lock = GitFileLock::acquire(&exclude).map_err(ValidationError::Io)?;
        let mut bytes = match fs_safety::open_beneath(&lock.parent, Path::new("exclude")) {
            Ok(mut file) => fs_safety::read_bounded(&mut file, 4 * 1024 * 1024).map_err(|error| ValidationError::Io(error.to_string()))?,
            Err(error) if error.is_not_found() => Vec::new(),
            Err(error) => return Err(ValidationError::Io(error.to_string()).into()),
        };
        let mut pattern = vec![b'/'];
        for byte in relative.as_os_str().as_bytes() {
            if matches!(byte, b'\\' | b'!' | b'#' | b'*' | b'?' | b'[' | b']' | b' ') { pattern.push(b'\\'); }
            if matches!(byte, b'\n' | b'\r') { return Err(ValidationError::Io("Managed worktree directory contains a newline".into()).into()); }
            pattern.push(*byte);
        }
        pattern.push(b'/');
        if bytes.split(|byte| *byte == b'\n').any(|line| line == pattern) { return Ok(()); }
        if !bytes.is_empty() && !bytes.ends_with(b"\n") { bytes.push(b'\n'); }
        bytes.extend_from_slice(&pattern);
        bytes.push(b'\n');
        lock.file.write_all(&bytes).and_then(|_| lock.file.sync_all()).map_err(|error| ValidationError::Io(error.to_string()))?;
        lock.version = GitFileLock::version(&lock.file.metadata().map_err(|error| ValidationError::Io(error.to_string()))?);
        lock.check().map_err(ValidationError::Io)?;
        fs_safety::rename_within(&lock.parent, &lock.name, OsStr::new("exclude")).map_err(|error| ValidationError::Io(error.to_string()))?;
        lock.active = false;
        fs::File::open(&info).and_then(|file| file.sync_all()).map_err(|error| ValidationError::Io(error.to_string()))?;
        Ok(())
    }

    /// Existing legacy worktrees stay in place while their directory stops
    /// polluting the primary checkout on the next managed creation.
    pub fn ignore_legacy_managed_worktrees(&self, repository: &Path, cancellation: &CancellationToken) -> Result<(), RepositoryError> {
        let records = self.list_worktrees(repository, cancellation)?;
        if let Some(primary) = records.first() {
            let legacy = primary.path.join("worktrees");
            if records.iter().skip(1).any(|record| record.path.starts_with(&legacy)) {
                self.ensure_managed_directory_ignored(repository, &legacy, cancellation)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git").arg("-C").arg(root).args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"]).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr)); output.stdout
    }
    #[test]
    fn managed_exclusion_is_local_atomic_idempotent_and_skips_external_or_existing_rules() {
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = fixture.path().join("repo"); fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]); git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        let service = RepositoryService::system("git"); let cancel = CancellationToken::new();
        let external = fixture.path().join("external");
        let original = fs::read(root.join(".git/info/exclude")).unwrap();
        service.ensure_managed_directory_ignored(&root, &external, &cancel).unwrap();
        assert_eq!(fs::read(root.join(".git/info/exclude")).unwrap(), original);
        let managed = root.join(".worktrees");
        service.ensure_managed_directory_ignored(&root, &managed, &cancel).unwrap();
        let once = fs::read(root.join(".git/info/exclude")).unwrap();
        service.ensure_managed_directory_ignored(&root, &managed, &cancel).unwrap();
        assert_eq!(fs::read(root.join(".git/info/exclude")).unwrap(), once);
        fs::create_dir_all(&managed).unwrap();
        git(&root, &["worktree", "add", "-b", "feature", managed.join("feature").to_str().unwrap()]);
        assert!(git(&root, &["status", "--porcelain", "-uall"]).is_empty());
        let another = root.join("already"); fs::write(root.join(".gitignore"), "already/\n").unwrap();
        service.ensure_managed_directory_ignored(&root, &another, &cancel).unwrap();
        assert_eq!(fs::read(root.join(".git/info/exclude")).unwrap(), once);
        // Legacy worktrees are excluded without relocating user data.
        let legacy = root.join("worktrees/old");
        git(&root, &["worktree", "add", "-b", "old", legacy.to_str().unwrap()]);
        service.ignore_legacy_managed_worktrees(&root, &cancel).unwrap();
        assert!(legacy.exists());
        assert!(!String::from_utf8(git(&root, &["status", "--porcelain", "-uall"])).unwrap().contains("worktrees/"));
    }
}
