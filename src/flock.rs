//! Authenticate exclusive flock owners using their exact locked descriptors.
use std::{fs, io, os::unix::fs::MetadataExt, path::Path};

pub(crate) fn flock_owner(line: &str, metadata: &fs::Metadata) -> Option<u32> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != 8
        || fields[1..4] != ["FLOCK", "ADVISORY", "WRITE"]
        || fields[6..8] != ["0", "EOF"]
    {
        return None;
    }
    let identity: Vec<_> = fields[5].split(':').collect();
    if identity.len() != 3
        || u64::from_str_radix(identity[0], 16).is_err()
        || u64::from_str_radix(identity[1], 16).is_err()
        || identity[2].parse::<u64>().ok() != Some(metadata.ino())
    {
        return None;
    }
    fields[4].parse::<u32>().ok().filter(|pid| *pid > 1)
}

pub(crate) fn fdinfo_owns_lock(info: &str, pid: u32, metadata: &fs::Metadata) -> bool {
    info.lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .any(|line| flock_owner(line, metadata) == Some(pid))
}

pub(crate) fn process_owns_lock(
    process: &Path,
    pid: u32,
    expected: &fs::Metadata,
) -> io::Result<bool> {
    Ok(fs::read_dir(process.join("fdinfo"))?
        .filter_map(Result::ok)
        .any(|entry| {
            let descriptor = process.join("fd").join(entry.file_name());
            let matches = |actual: fs::Metadata| {
                actual.dev() == expected.dev() && actual.ino() == expected.ino()
            };
            fs::metadata(&descriptor).is_ok_and(&matches)
                && fs::read_to_string(entry.path())
                    .is_ok_and(|info| fdinfo_owns_lock(&info, pid, expected))
                && fs::metadata(&descriptor).is_ok_and(matches)
        }))
}

/// `/proc/locks` is an inode index, not a file identity proof: Btrfs prints
/// the superblock device there, while stat reports the subvolume device.
/// Disambiguate candidates with fdinfo and stat of the same descriptor.
pub(crate) fn holder_pid(proc_root: &Path, expected: &fs::Metadata) -> io::Result<Option<u32>> {
    let locks = fs::read_to_string(proc_root.join("locks"))?;
    for pid in locks.lines().filter_map(|line| flock_owner(line, expected)) {
        let process = proc_root.join(pid.to_string());
        // Inode collisions can belong to another user's filesystem. Exclude
        // those owners before their private fdinfo can refuse inspection.
        match fs::metadata(&process) {
            Ok(metadata) if metadata.uid() != expected.uid() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        match process_owns_lock(&process, pid, expected) {
            Ok(true) => return Ok(Some(pid)),
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const PID: u32 = 42;

    fn lock_record(metadata: &fs::Metadata) -> String {
        // Deliberately differ from st_dev, as on Btrfs subvolumes.
        format!(
            "1: FLOCK ADVISORY WRITE {PID} {:02x}:{:02x}:{} 0 EOF\n",
            libc::major(metadata.dev()),
            libc::minor(metadata.dev()) ^ 1,
            metadata.ino(),
        )
    }

    fn fake_process(proc_root: &Path, record: &str) -> std::path::PathBuf {
        let process = proc_root.join(PID.to_string());
        fs::create_dir_all(process.join("fdinfo")).unwrap();
        fs::create_dir(process.join("fd")).unwrap();
        fs::write(proc_root.join("locks"), record).unwrap();
        process
    }

    #[test]
    fn lease_handoff_finds_holder_when_kernel_device_differs_from_stat() {
        let root = tempfile::tempdir().unwrap();
        let lock = tempfile::NamedTempFile::new().unwrap();
        let expected = lock.as_file().metadata().unwrap();
        let record = lock_record(&expected);
        let process = fake_process(root.path(), &record);
        symlink(lock.path(), process.join("fd/7")).unwrap();
        fs::write(process.join("fdinfo/7"), format!("lock: {record}")).unwrap();
        assert_eq!(holder_pid(root.path(), &expected).unwrap(), Some(PID));
    }

    #[test]
    fn lease_handoff_skips_foreign_candidate_before_authenticating_its_owner() {
        let root = tempfile::tempdir().unwrap();
        let lock = tempfile::NamedTempFile::new().unwrap();
        let expected = lock.as_file().metadata().unwrap();
        // An ordinary user cannot enumerate init's fdinfo. Root has no
        // foreign-owner permission boundary to exercise with this fixture.
        if fs::metadata("/proc/1").unwrap().uid() == expected.uid() {
            return;
        }
        let record = lock_record(&expected);
        let foreign = record.replace(&format!("WRITE {PID} "), "WRITE 41 ");
        let process = fake_process(root.path(), &format!("{foreign}{record}"));
        symlink("/proc/1", root.path().join("41")).unwrap();
        symlink(lock.path(), process.join("fd/7")).unwrap();
        fs::write(process.join("fdinfo/7"), format!("lock: {record}")).unwrap();
        assert_eq!(holder_pid(root.path(), &expected).unwrap(), Some(PID));
    }

    #[test]
    fn lease_handoff_rejects_same_inode_on_another_device() {
        let root = tempfile::tempdir().unwrap();
        // These Linux filesystem roots share inode 1 on distinct devices.
        // No mounts or changes to either filesystem are needed for the fixture.
        let expected = fs::metadata("/proc").unwrap();
        let unrelated = fs::metadata("/sys").unwrap();
        assert_eq!(expected.ino(), unrelated.ino());
        assert_ne!(expected.dev(), unrelated.dev());
        let record = lock_record(&expected);
        let process = fake_process(root.path(), &record);
        // The flock record matches the indexed inode, but fd stat proves it
        // belongs to another device. An unlocked target fd cannot supply proof.
        symlink("/sys", process.join("fd/7")).unwrap();
        fs::write(process.join("fdinfo/7"), format!("lock: {record}")).unwrap();
        symlink("/proc", process.join("fd/8")).unwrap();
        fs::write(process.join("fdinfo/8"), "pos: 0\n").unwrap();
        assert!(!process_owns_lock(&process, PID, &expected).unwrap());
        assert_eq!(holder_pid(root.path(), &expected).unwrap(), None);
        // Nor can an unrelated flock in the correct descriptor authenticate it.
        let other_inode = record.replace(&format!(":{} ", expected.ino()), ":2 ");
        fs::write(process.join("fdinfo/8"), format!("lock: {other_inode}")).unwrap();
        assert!(!process_owns_lock(&process, PID, &expected).unwrap());
        assert_eq!(holder_pid(root.path(), &expected).unwrap(), None);
    }
}
