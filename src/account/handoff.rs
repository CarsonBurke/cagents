//! Copy provider transcripts between CLI homes, preserving their relative paths.
use crate::{agent::Provider, chat::Session, fs_safety};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Seek, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_TRANSCRIPT: u64 = 256 * 1024 * 1024;
/// Names a staged copy beside its destination, which no provider reads.
const STAGED_PREFIX: &str = ".cagents-handoff-";
/// How old a staged copy is before it counts as abandoned.
const ABANDONED: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// A conversation's history copied beside its place on the alternate account
/// while the original conversation still runs. [`Staged::commit`] publishes
/// the final history, adding anything written after staging. Dropping an
/// unpublished stage removes its copy.
#[must_use = "a staged transcript is removed unless it is committed"]
pub struct Staged {
    source: PathBuf,
    destination: PathBuf,
    /// The copy, until it is renamed into place or removed.
    temporary: Option<Temporary>,
}

/// Stages `source`'s history for `replacement`'s account. It refuses an
/// alternate account whose copy of the conversation has diverged.
pub fn stage(source: &Session, replacement: &Session) -> Result<Staged, String> {
    let (source, destination) = locate(source, replacement)?;
    stage_transcript(source, destination, Staging::Active).map_err(|error| {
        format!("The conversation history could not be prepared on the alternate account: {error}")
    })
}

enum Publication {
    Synced,
    Visible(io::Error),
}

impl Staged {
    /// Publish and sync before an account removal can proceed.
    pub fn commit(mut self) -> Result<(), String> {
        let publication = commit_transcript(&mut self).map_err(commit_error)?;
        match publication {
            Publication::Synced => Ok(()),
            Publication::Visible(error) => Err(commit_error(error)),
        }
    }

    /// Publish the final history after the original provider has closed.
    pub fn commit_after_close(mut self) -> Result<(), String> {
        if let Publication::Visible(error) = commit_transcript(&mut self).map_err(commit_error)? {
            // The exact destination can already be opened. A directory sync
            // error affects crash durability; the live switch can continue.
            tracing::warn!(%error, "Published account history could not be synced to the directory");
        }
        Ok(())
    }
}

fn commit_error(error: io::Error) -> String {
    format!("The conversation history could not be moved to the alternate account: {error}")
}

struct Temporary {
    path: PathBuf,
    identity: (u64, u64),
}

impl Temporary {
    fn parent(&self) -> io::Result<fs_safety::Root> {
        open_parent(&self.path)
    }

    fn name(&self) -> &OsStr {
        self.path.file_name().expect("stage has a file name")
    }

    fn require_link(&self, parent: &fs_safety::Root) -> io::Result<()> {
        if still_named(parent, self.name(), self.identity) {
            Ok(())
        } else {
            Err(io::Error::other(
                "The staged history was replaced or removed",
            ))
        }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if let Ok(parent) = self.parent()
            && self.require_link(&parent).is_ok()
        {
            let _ = fs_safety::unlink_within(&parent, self.name());
        }
    }
}

fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn still_named(parent: &fs_safety::Root, name: &OsStr, expected: (u64, u64)) -> bool {
    fs_safety::metadata_beneath(parent, Path::new(name))
        .is_ok_and(|named| named.is_file() && identity(&named) == expected)
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
        Provider::Antigravity | Provider::Muse => {
            return Err(format!("{} manages its own logins", provider.label()));
        }
    };
    let relative = path
        .strip_prefix(&original)
        .map_err(|_| "The transcript escapes the original account home")?;
    let destination = target.join(relative);
    Ok((path, destination))
}

fn open_transcript(path: &Path) -> io::Result<fs::File> {
    crate::secure_file::reject_symlink_components(path)?;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

fn bounded_transcript(file: &fs::File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT {
        return Err(io::Error::other("Transcript is not a bounded regular file"));
    }
    Ok(())
}

fn read_transcript(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_transcript(path)?;
    bounded_transcript(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSCRIPT {
        return Err(io::Error::other(
            "Transcript grew beyond the account-switch limit",
        ));
    }
    Ok(bytes)
}

/// How much of `bytes` the open file already holds, or None if it diverges.
fn existing_prefix(file: &mut fs::File, bytes: &[u8]) -> io::Result<Option<usize>> {
    let mut previous = file.take(MAX_TRANSCRIPT + 1);
    let mut buffer = [0; 16 * 1024];
    let mut length = 0;
    loop {
        let count = match previous.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(Some(length));
        }
        let end = length + count;
        if bytes.get(length..end) != Some(&buffer[..count]) {
            return Ok(None);
        }
        length = end;
    }
}

#[derive(PartialEq, Eq)]
enum Destination {
    Missing,
    Prefix,
    Identical,
}

/// A divergent or longer destination belongs to the alternate account and
/// is kept intact.
fn destination_status(file: io::Result<fs::File>, bytes: &[u8]) -> io::Result<Destination> {
    match file.and_then(|file| {
        bounded_transcript(&file)?;
        Ok(file)
    }) {
        Ok(mut file) => match existing_prefix(&mut file, bytes)? {
            Some(length) if length == bytes.len() => Ok(Destination::Identical),
            Some(_) => Ok(Destination::Prefix),
            None => Err(io::Error::other(
                "Alternate account has divergent conversation history; refusing to overwrite it",
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Destination::Missing),
        Err(error) => Err(error),
    }
}

enum Staging {
    Active,
    Idle { clean: bool },
}

fn stage_transcript(source: PathBuf, destination: PathBuf, staging: Staging) -> io::Result<Staged> {
    let bytes = read_transcript(&source)?;
    let identical =
        destination_status(open_transcript(&destination), &bytes)? == Destination::Identical;
    let temporary = match (identical, staging) {
        (true, Staging::Idle { .. }) => None,
        // A live provider can write during shutdown. Stage a copy when
        // possible so a final append does not need space for the full history.
        // Matching history already works on a read-only destination.
        (true, Staging::Active) => write_stage(&destination, &bytes, false).ok(),
        (false, staging) => {
            let clean = matches!(staging, Staging::Active | Staging::Idle { clean: true });
            Some(write_stage(&destination, &bytes, clean)?)
        }
    };
    Ok(Staged {
        source,
        destination,
        temporary,
    })
}

fn write_stage(destination: &Path, bytes: &[u8], clean: bool) -> io::Result<Temporary> {
    let (temporary, _, mut file) = prepare_copy(destination, clean)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(temporary)
}

fn parent_path(destination: &Path) -> io::Result<&Path> {
    destination
        .parent()
        .ok_or_else(|| io::Error::other("Missing transcript directory"))
}

fn open_parent(destination: &Path) -> io::Result<fs_safety::Root> {
    let path = parent_path(destination)?;
    crate::secure_file::reject_symlink_components(path)?;
    let parent = fs_safety::Root::open_local(path)?;
    sweep::require_owned(&parent)?;
    Ok(parent)
}

fn prepare_copy(
    destination: &Path,
    clean: bool,
) -> io::Result<(Temporary, fs_safety::Root, fs::File)> {
    let path = parent_path(destination)?;
    crate::secure_file::reject_symlink_components(path)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let parent = open_parent(destination)?;
    if clean {
        let _ = sweep::copies_in(&parent, 0);
    }
    let name = sweep::stage_name()?;
    let raw = std::ffi::CString::new(name.as_bytes()).expect("stage name contains no NUL bytes");
    let file = fs::File::from(crate::descriptor::openat(
        parent.descriptor(),
        &raw,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )?);
    let temporary = Temporary {
        path: parent.path().join(name),
        identity: identity(&file.metadata()?),
    };
    Ok((temporary, parent, file))
}

fn commit_transcript(staged: &mut Staged) -> io::Result<Publication> {
    let bytes = read_transcript(&staged.source)?;
    if destination_status(open_transcript(&staged.destination), &bytes)? == Destination::Identical {
        return Ok(Publication::Synced);
    }
    // If identical history could not be staged, a final append needs a
    // fresh copy. An unchanged identical destination returned above.
    let (parent, mut file) = match &staged.temporary {
        Some(temporary) => {
            let parent = temporary.parent()?;
            temporary.require_link(&parent)?;
            let file = fs_safety::open_beneath_writable(&parent, Path::new(temporary.name()))
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("The staged history is no longer writable"))?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || identity(&metadata) != temporary.identity
                || metadata.nlink() != 1
                || metadata.len() > MAX_TRANSCRIPT
            {
                return Err(io::Error::other(
                    "The staged history is no longer a bounded regular file",
                ));
            }
            (parent, file)
        }
        None => {
            let (temporary, parent, file) = prepare_copy(&staged.destination, false)?;
            staged.temporary = Some(temporary);
            (parent, file)
        }
    };
    // Reuse a matching prefix on the held descriptor. A shortened or rewritten
    // source replaces the staged bytes on that same descriptor.
    let written = match existing_prefix(&mut file, &bytes)? {
        Some(written) => written,
        None => {
            file.set_len(0)?;
            file.rewind()?;
            0
        }
    };
    file.write_all(&bytes[written..])?;
    file.sync_all()?;
    let publication = publish(
        staged.temporary.as_ref().unwrap(),
        &parent,
        &staged.destination,
        &bytes,
    )?;
    staged.temporary = None;
    Ok(publication)
}

fn publish(
    temporary: &Temporary,
    parent: &fs_safety::Root,
    destination: &Path,
    bytes: &[u8],
) -> io::Result<Publication> {
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("Missing transcript name"))?;
    // Check the destination through the same directory as the rename. A
    // concurrent writer may have changed it while the staged copy was synced.
    let existing = fs_safety::open_beneath(parent, Path::new(name)).map_err(|error| {
        if error.is_not_found() {
            io::Error::new(io::ErrorKind::NotFound, error)
        } else {
            io::Error::other(error)
        }
    });
    let status = destination_status(existing, bytes)?;
    if identity(&parent.metadata()?) != identity(&open_parent(destination)?.metadata()?) {
        return Err(io::Error::other(
            "The alternate account history directory changed",
        ));
    }
    let rename = match status {
        Destination::Identical => return Ok(Publication::Synced),
        Destination::Missing => fs_safety::rename_new_within,
        Destination::Prefix => fs_safety::rename_within,
    };
    temporary.require_link(parent)?;
    rename(parent, temporary.name(), name).map_err(io::Error::other)?;
    Ok(match fs_safety::sync_directory(parent) {
        Ok(()) => Publication::Synced,
        Err(error) => Publication::Visible(error),
    })
}

mod sweep;
pub use sweep::sweep_codex;

#[cfg(feature = "host")]
mod inventory;
#[cfg(feature = "host")]
pub(crate) use inventory::Inventory;

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_at(source: &Path, destination: &Path) -> io::Result<Staged> {
        stage_transcript(source.to_owned(), destination.to_owned(), Staging::Active)
    }

    fn copy_transcript(source: &Path, destination: &Path) -> io::Result<()> {
        stage_at(source, destination)?
            .commit()
            .map_err(io::Error::other)
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
        let parent = target.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o550)).unwrap();
        let before = fs::metadata(&target).unwrap();
        let staged = stage_at(&source, &target).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(staged.temporary.is_none());
        }
        staged.commit().unwrap();
        let after = fs::metadata(&target).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.mode(), before.mode());
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
        assert_eq!(fs::metadata(parent).unwrap().mode() & 0o777, 0o550);
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
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

        // A writable identical destination is pre-staged in case shutdown
        // appends to the source history.
        let mut staged = stage_at(&source, &rewritten).unwrap();
        assert!(staged.temporary.is_some());
        fs::write(&source, b"other\nmore\n").unwrap();
        commit_transcript(&mut staged).unwrap();
        drop(staged);
        assert_eq!(fs::read(&rewritten).unwrap(), b"other\nmore\n");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 2);

        // History that diverged on the alternate account since staging stays.
        fs::write(&source, b"one\ntwo\nthree\n").unwrap();
        let mut staged = stage_at(&source, &target).unwrap();
        fs::write(&target, b"elsewhere\n").unwrap();
        let Err(error) = commit_transcript(&mut staged) else {
            panic!("Divergent destination was accepted");
        };
        assert!(error.to_string().contains("divergent conversation history"));
        drop(staged);
        assert_eq!(fs::read(&target).unwrap(), b"elsewhere\n");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn commit_truncates_a_staged_copy_to_the_shortened_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        fs::write(&source, b"one\ntwo\n").unwrap();
        let staged = stage_at(&source, &target).unwrap();
        fs::write(&source, b"one\n").unwrap();

        staged.commit().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"one\n");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn source_growth_beyond_limit_leaves_previous_history_and_removes_stage() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        fs::write(&source, b"one\ntwo\n").unwrap();
        fs::write(&target, b"one\n").unwrap();
        let staged = stage_at(&source, &target).unwrap();
        let temporary = staged.temporary.as_ref().unwrap().path.clone();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(MAX_TRANSCRIPT + 1)
            .unwrap();

        assert!(
            staged
                .commit()
                .unwrap_err()
                .contains("bounded regular file")
        );
        assert_eq!(fs::metadata(&source).unwrap().len(), MAX_TRANSCRIPT + 1);
        assert_eq!(fs::read(&target).unwrap(), b"one\n");
        assert!(!temporary.exists());
    }

    #[test]
    fn commit_refuses_a_fifo_substituted_for_the_staged_copy() {
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.jsonl");
        let target = directory.path().join("target.jsonl");
        fs::write(&source, b"history\n").unwrap();
        let staged = stage_at(&source, &target).unwrap();
        let temporary = staged.temporary.as_ref().unwrap().path.clone();
        let original = temporary.with_extension("saved");
        // Keep the original inode allocated to rule out immediate inode reuse.
        fs::rename(&temporary, &original).unwrap();
        let name = std::ffi::CString::new(temporary.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated pathname outlives this call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        assert!(staged.commit().unwrap_err().contains("replaced or removed"));
        assert!(!target.exists());
        assert_eq!(fs::read(original).unwrap(), b"history\n");
        assert!(temporary.exists());
    }

    #[test]
    fn publication_rechecks_the_destination_after_the_copy_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.jsonl");
        let bytes = b"original history\n";
        assert!(
            destination_status(open_transcript(&target), bytes).unwrap() == Destination::Missing
        );
        let temporary = write_stage(&target, bytes, false).unwrap();
        let parent = temporary.parent().unwrap();

        fs::write(&target, b"another writer\n").unwrap();
        let Err(error) = publish(&temporary, &parent, &target, bytes) else {
            panic!("Divergent destination was published");
        };
        assert!(error.to_string().contains("divergent conversation history"));
        assert_eq!(fs::read(target).unwrap(), b"another writer\n");
    }
}
