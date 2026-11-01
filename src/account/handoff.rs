//! Transfer only the exact idle conversation's transcript between CLI homes.
//! Login stores, configuration and unrelated conversations never move.
use crate::{agent::Provider, chat::Session};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_TRANSCRIPT: u64 = 256 * 1024 * 1024;
const ENTRY_BUDGET: usize = 262_144;

pub fn prepare(source: &Session, replacement: &Session) -> Result<(), String> {
    transfer(source, replacement, true)
}

pub fn preflight(source: &Session, replacement: &Session) -> Result<(), String> {
    transfer(source, replacement, false)
}

fn transfer(source: &Session, replacement: &Session, commit: bool) -> Result<(), String> {
    let provider = source.launch.provider;
    let id = source
        .launch
        .session_id
        .as_deref()
        .ok_or("The conversation has no confirmed identity")?;
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err("Automatic account switching requires an exact provider UUID".into());
    }
    let original = provider
        .session_root(&source.launch.recipe)
        .ok_or("Cannot locate the source account history")?;
    let target = provider
        .session_root(&replacement.launch.recipe)
        .ok_or("Cannot locate the alternate account history")?;
    if !original.is_absolute() || !target.is_absolute() {
        return Err("Account switching needs absolute provider home paths".into());
    }
    let original = original.canonicalize().map_err(|error| error.to_string())?;
    let path = match provider {
        Provider::Claude => {
            crate::agent::claude_transcript(&original, &source.launch.recipe.working_directory, id)?
        }
        Provider::Codex => {
            let mut found = Vec::new();
            let mut budget = ENTRY_BUDGET;
            find(
                &original.join("sessions"),
                4,
                &format!("-{id}.jsonl"),
                &mut budget,
                &mut found,
            )?;
            if found.len() != 1 {
                return Err("The exact Codex transcript is missing or ambiguous; the conversation stays on its original account".into());
            }
            found.pop().unwrap()
        }
        Provider::Antigravity => {
            return Err("Antigravity has one sign-in on this computer".into());
        }
        Provider::Omp => return Err("Omp manages its own account rotation".into()),
    };
    let relative = path
        .strip_prefix(&original)
        .map_err(|_| "The transcript escapes the original account home")?;
    copy_transcript(&path, &target.join(relative), commit).map_err(|error| {
        format!("The conversation history could not be prepared on the alternate account: {error}")
    })
}

fn find(
    root: &Path,
    depth: usize,
    suffix: &str,
    budget: &mut usize,
    found: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if depth == 0 {
        return Ok(());
    }
    crate::secure_file::reject_symlink_components(root).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        if *budget == 0 {
            return Err("The provider history exceeds the bounded account-switch scan".into());
        }
        *budget -= 1;
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            find(&entry.path(), depth - 1, suffix, budget, found)?;
        } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
            found.push(entry.path());
        }
    }
    Ok(())
}

fn open_transcript(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::secure_file::reject_symlink_components(path)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT {
        return Err(io::Error::other("Transcript is not a bounded regular file"));
    }
    Ok(file)
}

fn read_transcript(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_transcript(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSCRIPT {
        return Err(io::Error::other(
            "Transcript grew beyond the account-switch limit",
        ));
    }
    Ok(bytes)
}

// Retain the source snapshot until publication, while comparing the destination
// in fixed-size chunks instead of retaining a second transcript-sized buffer.
fn matches_existing_transcript(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let mut previous = open_transcript(path)?.take(MAX_TRANSCRIPT + 1);
    let mut buffer = [0; 16 * 1024];
    let mut length = 0;
    let mut matches = true;
    loop {
        let count = match previous.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        let end = length + count;
        if end as u64 > MAX_TRANSCRIPT {
            return Err(io::Error::other(
                "Transcript grew beyond the account-switch limit",
            ));
        }
        matches &= bytes.get(length..end) == Some(&buffer[..count]);
        length = end;
    }
    if !matches {
        return Err(io::Error::other(
            "Alternate account has divergent conversation history; refusing to overwrite it",
        ));
    }
    Ok(length == bytes.len())
}

fn copy_transcript(source: &Path, destination: &Path, commit: bool) -> io::Result<()> {
    let bytes = read_transcript(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("Missing transcript directory"))?;
    crate::secure_file::reject_symlink_components(destination)?;
    if destination.exists() && matches_existing_transcript(destination, &bytes)? {
        return Ok(());
    }
    if !commit {
        return Ok(());
    }
    crate::secure_file::prepare_private_directory(parent)?;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = parent.join(format!(".cagents-handoff-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, destination)?;
        fs::File::open(parent)?.sync_all()
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_only_history_and_refuses_divergence_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("account/sessions/target.jsonl");
        fs::write(&source, b"one\ntwo\n").unwrap();
        copy_transcript(&source, &target, true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\n");
        fs::write(&source, b"one\ntwo\nthree\n").unwrap();
        copy_transcript(&source, &target, true).unwrap();
        fs::write(&source, b"other\n").unwrap();
        assert!(copy_transcript(&source, &target, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\nthree\n");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&source, &target).unwrap();
        assert!(copy_transcript(&source, &target, true).is_err());
    }

    #[test]
    fn long_matching_history_extends_exactly_and_identical_copy_preserves_the_entry() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("account/sessions/target.jsonl");
        let bytes: Vec<u8> = (0..160 * 1024)
            .map(|offset| ((offset / 997) ^ offset) as u8)
            .collect();
        fs::write(&source, &bytes).unwrap();
        copy_transcript(&source, &target, false).unwrap();
        assert!(
            !target.parent().unwrap().exists(),
            "preflight must stay read-only"
        );

        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, &bytes[..96 * 1024]).unwrap();
        copy_transcript(&source, &target, false).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes[..96 * 1024]);
        copy_transcript(&source, &target, true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);

        fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
        let before = fs::metadata(&target).unwrap();
        copy_transcript(&source, &target, true).unwrap();
        let after = fs::metadata(&target).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.mode(), before.mode(), "identical history is a no-op");
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn late_history_divergence_and_a_longer_destination_preserve_existing_bytes() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        let bytes = vec![b'x'; 192 * 1024];
        let mut divergent = bytes.clone();
        *divergent.last_mut().unwrap() = b'y';
        fs::write(&source, &bytes).unwrap();
        for previous in [divergent, [bytes.as_slice(), b"extra"].concat()] {
            fs::write(&target, &previous).unwrap();
            let before = fs::metadata(&target).unwrap();
            for commit in [false, true] {
                let error = copy_transcript(&source, &target, commit).unwrap_err();
                assert!(error.to_string().contains("divergent conversation history"));
                assert_eq!(fs::read(&target).unwrap(), previous);
                assert_eq!(fs::metadata(&target).unwrap().ino(), before.ino());
                assert_eq!(fs::read(&source).unwrap(), bytes);
                assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
            }
        }
    }

    #[test]
    fn existing_history_comparison_refuses_fifo_directory_and_oversized_files() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        let fifo = directory.path().join("fifo.jsonl");
        let oversized = directory.path().join("oversized.jsonl");
        fs::write(&source, b"source\n").unwrap();
        fs::write(&target, b"retained\n").unwrap();
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_TRANSCRIPT + 1)
            .unwrap();
        for rejected in [fifo.as_path(), directory.path(), oversized.as_path()] {
            assert!(copy_transcript(&source, rejected, false).is_err());
            assert!(copy_transcript(rejected, &target, false).is_err());
        }
        assert_eq!(fs::read(&target).unwrap(), b"retained\n");
        assert_eq!(fs::read(&source).unwrap(), b"source\n");
        assert_eq!(fs::metadata(&oversized).unwrap().len(), MAX_TRANSCRIPT + 1);
    }
}
