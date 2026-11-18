//! Transfer only the exact idle conversation's transcript between CLI homes.
//! Login stores, configuration and unrelated conversations never move.
use crate::{agent::Provider, chat::Session};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAX_TRANSCRIPT: u64 = 256 * 1024 * 1024;

/// A conversation's history copied beside its place on the alternate account
/// while the original conversation still runs. Staging is what can fail for
/// lack of space or permission, so it happens before the original closes;
/// [`Staged::commit`] then adds only what the conversation wrote since and
/// renames the copy into place. Dropping it uncommitted removes the copy.
#[must_use = "a staged transcript is removed unless it is committed"]
pub struct Staged {
    source: PathBuf,
    destination: PathBuf,
    /// The copy, absent when the destination already held this history.
    temporary: Option<PathBuf>,
}

/// Stages `source`'s history for `replacement`'s account. It refuses an
/// alternate account whose copy of the conversation has diverged.
pub fn stage(source: &Session, replacement: &Session) -> Result<Staged, String> {
    let (source, destination) = locate(source, replacement)?;
    stage_transcript(source, destination).map_err(|error| {
        format!("The conversation history could not be prepared on the alternate account: {error}")
    })
}

impl Staged {
    /// Publishes the history once the original conversation has closed,
    /// including anything it wrote after staging.
    pub fn commit(mut self) -> Result<(), String> {
        commit_transcript(&mut self).map_err(|error| {
            format!("The conversation history could not be moved to the alternate account: {error}")
        })
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            let _ = fs::remove_file(temporary);
        }
    }
}

/// The source transcript and its place under the alternate account's home.
fn locate(source: &Session, replacement: &Session) -> Result<(PathBuf, PathBuf), String> {
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
        Provider::Codex => crate::agent::codex_rollout(&original, id)
            .map_err(|error| format!("{error}; the conversation stays on its original account"))?,
        Provider::Omp => return Err("Omp manages its own account rotation".into()),
    };
    let relative = path
        .strip_prefix(&original)
        .map_err(|_| "The transcript escapes the original account home")?;
    let destination = target.join(relative);
    Ok((path, destination))
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

// Compare the destination in fixed-size chunks instead of retaining a second
// transcript-sized buffer.
fn matches_existing_transcript(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    Ok(existing_prefix(path, bytes)? == bytes.len())
}

/// How much of `bytes` the file at `path` already holds, refusing a file that
/// is not a prefix of them.
fn existing_prefix(path: &Path, bytes: &[u8]) -> io::Result<usize> {
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
    Ok(length)
}

fn transcript_directory(destination: &Path) -> io::Result<&Path> {
    destination
        .parent()
        .ok_or_else(|| io::Error::other("Missing transcript directory"))
}

/// Whether the destination already holds exactly `bytes`. A destination that
/// diverges from them, or is longer, is refused rather than overwritten.
fn destination_current(destination: &Path, bytes: &[u8]) -> io::Result<bool> {
    crate::secure_file::reject_symlink_components(destination)?;
    Ok(destination.exists() && matches_existing_transcript(destination, bytes)?)
}

fn stage_transcript(source: PathBuf, destination: PathBuf) -> io::Result<Staged> {
    let bytes = read_transcript(&source)?;
    let mut staged = Staged {
        source,
        destination,
        temporary: None,
    };
    if destination_current(&staged.destination, &bytes)? {
        return Ok(staged);
    }
    let parent = transcript_directory(&staged.destination)?;
    crate::secure_file::prepare_private_directory(parent)?;
    let temporary = parent.join(format!(".cagents-handoff-{}", uuid::Uuid::new_v4()));
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    staged.temporary = Some(temporary);
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(staged)
}

fn commit_transcript(staged: &mut Staged) -> io::Result<()> {
    let bytes = read_transcript(&staged.source)?;
    if destination_current(&staged.destination, &bytes)? {
        return Ok(());
    }
    let parent = transcript_directory(&staged.destination)?.to_owned();
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    // The staged copy is normally all or nearly all of the history. Only a
    // source that changed what it had already written is copied again.
    let (temporary, mut file, written) = match staged.temporary.clone() {
        Some(temporary) => match existing_prefix(&temporary, &bytes) {
            Ok(written) => {
                let file = options.append(true).open(&temporary)?;
                (temporary, file, written)
            }
            Err(_) => {
                let file = options.write(true).truncate(true).open(&temporary)?;
                (temporary, file, 0)
            }
        },
        None => {
            crate::secure_file::prepare_private_directory(&parent)?;
            let temporary = parent.join(format!(".cagents-handoff-{}", uuid::Uuid::new_v4()));
            let file = options.write(true).create_new(true).open(&temporary)?;
            staged.temporary = Some(temporary.clone());
            (temporary, file, 0)
        }
    };
    file.write_all(&bytes[written..])?;
    file.sync_all()?;
    fs::rename(&temporary, &staged.destination)?;
    staged.temporary = None;
    fs::File::open(&parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_at(source: &Path, destination: &Path) -> io::Result<Staged> {
        stage_transcript(source.to_owned(), destination.to_owned())
    }

    fn copy_transcript(source: &Path, destination: &Path) -> io::Result<()> {
        commit_transcript(&mut stage_at(source, destination)?)
    }
    #[test]
    fn copies_only_history_and_refuses_divergence_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("account/sessions/target.jsonl");
        fs::write(&source, b"one\ntwo\n").unwrap();
        copy_transcript(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\n");
        fs::write(&source, b"one\ntwo\nthree\n").unwrap();
        copy_transcript(&source, &target).unwrap();
        fs::write(&source, b"other\n").unwrap();
        assert!(copy_transcript(&source, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\nthree\n");
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&source, &target).unwrap();
        assert!(copy_transcript(&source, &target).is_err());
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
        drop(stage_at(&source, &target).unwrap());
        assert!(!target.exists(), "an uncommitted stage publishes nothing");
        assert_eq!(
            fs::read_dir(target.parent().unwrap()).unwrap().count(),
            0,
            "an uncommitted stage leaves no copy behind"
        );

        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, &bytes[..96 * 1024]).unwrap();
        drop(stage_at(&source, &target).unwrap());
        assert_eq!(fs::read(&target).unwrap(), bytes[..96 * 1024]);
        copy_transcript(&source, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);

        fs::set_permissions(&target, fs::Permissions::from_mode(0o400)).unwrap();
        let before = fs::metadata(&target).unwrap();
        copy_transcript(&source, &target).unwrap();
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
                let error = if commit {
                    copy_transcript(&source, &target).unwrap_err()
                } else {
                    stage_at(&source, &target).map(drop).unwrap_err()
                };
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
            assert!(stage_at(&source, rejected).is_err());
            assert!(stage_at(rejected, &target).is_err());
        }
        assert_eq!(fs::read(&target).unwrap(), b"retained\n");
        assert_eq!(fs::read(&source).unwrap(), b"source\n");
        assert_eq!(fs::metadata(&oversized).unwrap().len(), MAX_TRANSCRIPT + 1);
    }

    #[test]
    fn an_unwritable_destination_fails_while_staging() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let account = directory.path().join("account");
        fs::write(&source, b"history\n").unwrap();
        fs::create_dir_all(&account).unwrap();
        // The destination's directory cannot be created, as on a full or
        // read-only file system.
        fs::set_permissions(&account, fs::Permissions::from_mode(0o500)).unwrap();
        let refused = stage_at(&source, &account.join("sessions/target.jsonl"));
        let created = account.join("sessions").exists();
        fs::set_permissions(&account, fs::Permissions::from_mode(0o700)).unwrap();
        // Root writes through the missing permission, as no daemon user can.
        if unsafe { libc::geteuid() } != 0 {
            assert!(
                refused.is_err(),
                "staging must prove the destination writable"
            );
            assert!(!created);
        }
    }

    #[test]
    fn commit_publishes_what_the_conversation_wrote_after_staging() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("account/sessions/target.jsonl");
        fs::write(&source, b"one\n").unwrap();
        let mut staged = stage_at(&source, &target).unwrap();
        assert!(!target.exists(), "staging publishes nothing");
        fs::write(&source, b"one\ntwo\n").unwrap();
        commit_transcript(&mut staged).unwrap();
        drop(staged);
        assert_eq!(fs::read(&target).unwrap(), b"one\ntwo\n");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);

        // A source that rewrote its staged history is copied again in full.
        let rewritten = directory.path().join("account/sessions/rewritten.jsonl");
        let mut staged = stage_at(&source, &rewritten).unwrap();
        fs::write(&source, b"other\n").unwrap();
        commit_transcript(&mut staged).unwrap();
        assert_eq!(fs::read(&rewritten).unwrap(), b"other\n");

        // History that diverged on the alternate account since staging stays.
        fs::write(&source, b"one\ntwo\nthree\n").unwrap();
        let mut staged = stage_at(&source, &target).unwrap();
        fs::write(&target, b"elsewhere\n").unwrap();
        let error = commit_transcript(&mut staged).unwrap_err();
        assert!(error.to_string().contains("divergent conversation history"));
        drop(staged);
        assert_eq!(fs::read(&target).unwrap(), b"elsewhere\n");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 2);
    }
}
