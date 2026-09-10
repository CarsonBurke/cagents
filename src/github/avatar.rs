//! Anonymous public GitHub avatars. No remote-provided URLs or credentials are used.
use super::{
    CancellationToken, CommandSpec, GitHubRepoKey, ProcessExecutor, SystemProcessExecutor,
};
use cosmic::iced::advanced::image::Handle;
use std::{
    io::{Cursor, Read},
    time::Duration,
};

const MAX_BYTES: usize = 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
            && !repository.owner.is_empty()
            && repository.owner.len() <= 39
            && repository
                .owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        .then(|| Self {
            host: repository.host.clone(),
            owner: repository.owner.to_ascii_lowercase(),
        })
    }
}

/// Called once per owner by a blocking worker; handles retain decoded 64px RGBA.
pub fn load(key: &OwnerKey) -> Option<Handle> {
    let _permit = PERMITS.acquire();
    let cache = crate::instance::project_dirs()?
        .cache_dir()
        .join("github-avatars");
    let path = cache.join(format!("{}-{}.image", key.host, key.owner));
    let cached = std::fs::File::open(&path).ok().and_then(|file| {
        let metadata = file.metadata().ok()?;
        if metadata.len() > MAX_BYTES as u64 {
            return None;
        }
        let fresh = metadata.modified().ok()?.elapsed().ok()? < CACHE_TTL;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        Some((decode(&bytes)?, fresh))
    });
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
        .execute(&command, &CancellationToken::default())
        .and_then(|output| output.require_success(&command.program))
        .ok()
        .and_then(|output| {
            let handle = decode(&output.stdout)?;
            if std::fs::create_dir_all(&cache).is_ok() {
                // Atomic cache publication; interrupted downloads cannot poison it.
                if let Ok(mut temporary) = tempfile::NamedTempFile::new_in(&cache) {
                    use std::io::Write;
                    if temporary.write_all(&output.stdout).is_ok() {
                        let _ = temporary.persist(&path);
                    }
                }
            }
            Some(handle)
        });
    downloaded.or_else(|| cached.map(|(handle, _)| handle))
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
