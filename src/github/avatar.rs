//! Anonymous public GitHub avatars. No remote-provided URLs or credentials are used.
use super::{
    CancellationToken, CommandSpec, GitHubRepoKey, ProcessExecutor, SystemProcessExecutor,
};
use cosmic::iced::advanced::image::Handle;
use std::{
    fs::{self, OpenOptions},
    io::{self, Cursor, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Once,
    time::{Duration, SystemTime},
};

const MAX_BYTES: usize = 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CACHE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CACHE_ENTRIES: usize = 128;
const TEMP_PREFIX: &str = ".avatar-";
const TEMP_SUFFIX_LEN: usize = 6;

static PERMITS: std::sync::LazyLock<super::CheckPermits> =
    std::sync::LazyLock::new(|| super::CheckPermits::new(4));
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerKey {
    host: String,
    owner: String,
}

impl OwnerKey {
    pub fn from_repository(repository: &GitHubRepoKey) -> Option<Self> {
        // Enterprise avatar endpoints vary and can expose private identities. Until
        // explicitly supported they use the folder fallback rather than contacting
        // an arbitrary host from a Git remote.
        (repository.host == "github.com"
            && repository.port.is_none_or(|port| port == 443)
            && valid_owner(&repository.owner))
        .then(|| Self {
            host: repository.host.clone(),
            owner: repository.owner.to_ascii_lowercase(),
        })
    }
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Called once per owner by a blocking worker; handles retain decoded 64px RGBA.
pub fn load(key: &OwnerKey) -> Option<Handle> {
    let cancellation = CancellationToken::new();
    let _permit = PERMITS.acquire(&cancellation)?;
    let cache = crate::instance::project_dirs()?
        .cache_dir()
        .join("github-avatars");
    static INITIAL_SWEEP: Once = Once::new();
    INITIAL_SWEEP.call_once(|| {
        if cache.is_dir() {
            let _ = cache_lock(&cache).and_then(|_lock| prune(&cache, SystemTime::now(), None));
        }
    });
    let path = cache.join(format!("{}-{}.image", key.host, key.owner));
    let cached = read(&path);
    if let Some((handle, true)) = &cached {
        return Some(handle.clone());
    }
    let url = format!("https://avatars.githubusercontent.com/{}?s=64", key.owner);
    // -q MUST be first: ignore .curlrc, including user-supplied auth/redirects.
    // No redirects, netrc, headers, cookies, credentials or GitHub token access.
    let command = CommandSpec::new("curl")
        .args([
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "5",
            "--max-time",
            "12",
            "--max-filesize",
            "1048576",
            "--url",
            &url,
        ])
        .timeout(Duration::from_secs(15))
        .output_limits(MAX_BYTES, 4096);
    let downloaded = SystemProcessExecutor
        .execute(&command, &cancellation)
        .and_then(|output| output.require_success(&command.program))
        .ok()
        .and_then(|output| {
            let handle = decode(&output.stdout)?;
            let _ = publish(&cache, &path, &output.stdout);
            Some(handle)
        });
    downloaded.or_else(|| cached.map(|(handle, _)| handle))
}

fn read(path: &Path) -> Option<(Handle, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    let age = metadata.modified().ok()?.elapsed().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 || age >= CACHE_RETENTION {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Some((decode(&bytes)?, age < CACHE_TTL))
}

fn cache_lock(cache: &Path) -> io::Result<crate::instance::OwnedLock> {
    crate::secure_file::prepare_private_directory(cache)?;
    crate::instance::owned_lock(&cache.join(".lock"), libc::LOCK_EX)
}

fn publish(cache: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let _lock = cache_lock(cache)?;
    prune(cache, SystemTime::now(), Some(path))?;
    // Keep the publication and its temporary file under the sweep lock.
    let mut temporary = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .rand_bytes(TEMP_SUFFIX_LEN)
        .tempfile_in(cache)?;
    temporary.write_all(bytes)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The caller holds the cache lock. Reserve a slot before publishing so an
/// interrupted write cannot leave more entries than the cache permits.
fn prune(cache: &Path, now: SystemTime, replacement: Option<&Path>) -> io::Result<()> {
    let uid = fs::symlink_metadata(cache)?.uid();
    let mut retained = Vec::new();
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let avatar = name
            .strip_prefix("github.com-")
            .and_then(|name| name.strip_suffix(".image"))
            .is_some_and(valid_owner);
        let temporary = name.strip_prefix(TEMP_PREFIX).is_some_and(|suffix| {
            suffix.len() == TEMP_SUFFIX_LEN
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if !avatar && !temporary {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let path = entry.path();
        let replacing = replacement == Some(path.as_path());
        if !metadata.is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
            if replacing {
                return Err(io::Error::other(
                    "Avatar cache destination is not an owned file",
                ));
            }
            continue;
        }
        let modified = metadata.modified()?;
        if temporary
            || metadata.len() > MAX_BYTES as u64
            || now
                .duration_since(modified)
                .is_ok_and(|age| age >= CACHE_RETENTION)
        {
            remove(&path)?;
        } else if !replacing {
            retained.push((modified, path));
        }
    }
    retained.sort_unstable();
    let slots = CACHE_ENTRIES - usize::from(replacement.is_some());
    for (_, path) in retained.into_iter().rev().skip(slots) {
        remove(&path)?;
    }
    Ok(())
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn decode(bytes: &[u8]) -> Option<Handle> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .ok()?
        .resize_exact(64, 64, image::imageops::FilterType::Triangle)
        .to_rgba8();
    Some(Handle::from_rgba(64, 64, image.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(cache: &Path, name: &str, modified: SystemTime) -> std::path::PathBuf {
        let path = cache.join(name);
        let file = fs::File::create(&path).unwrap();
        file.set_modified(modified).unwrap();
        path
    }

    #[test]
    fn sweep_removes_expired_avatars_and_abandoned_publications() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path();
        let lock = cache_lock(cache).unwrap();
        let now = SystemTime::now();
        let old = now - CACHE_RETENTION - Duration::from_secs(1);
        let aged = cached(cache, "github.com-aged.image", old);
        let stale = cached(cache, "github.com-stale.image", now - CACHE_TTL);
        let unrelated = cached(cache, "notes.image", old);
        let temporary = cached(cache, ".avatar-Abc123", now);
        let target = cached(cache, "keep.txt", old);
        let link = cache.join("github.com-linked.image");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        prune(cache, now, None).unwrap();
        drop(lock);

        assert!(!aged.exists());
        assert!(!temporary.exists());
        assert!(publish(cache, &link, b"avatar").is_err());
        assert!(link.is_symlink());
        for kept in [stale, unrelated, target, link] {
            assert!(kept.exists(), "{}", kept.display());
        }
    }

    #[test]
    fn publication_keeps_the_newest_avatars_within_the_entry_limit() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path();
        let now = SystemTime::now();
        let oldest = cached(cache, "github.com-oldest.image", now - CACHE_TTL);
        let most_recent: Vec<_> = (0..CACHE_ENTRIES - 1)
            .map(|index| cached(cache, &format!("github.com-owner-{index}.image"), now))
            .collect();
        let newest = cache.join("github.com-newest.image");

        publish(cache, &newest, b"avatar").unwrap();
        publish(cache, &newest, b"updated").unwrap();

        assert!(!oldest.exists());
        assert!(most_recent.iter().all(|path| path.exists()));
        assert_eq!(fs::read(newest).unwrap(), b"updated");
    }
}
