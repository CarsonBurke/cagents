//! Clipboard image data for the native chat composer.

use std::{
    borrow::Cow,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use cosmic::iced::clipboard::mime::AllowedMimeTypes;

/// An image offered by the system clipboard. The MIME type decides the file
/// extension; `save` checks the actual bytes before making an attachment.
#[derive(Debug, Clone)]
pub(super) struct Image {
    bytes: Vec<u8>,
    mime: String,
}

impl AllowedMimeTypes for Image {
    fn allowed() -> Cow<'static, [String]> {
        Cow::Owned(
            ["image/png", "image/jpeg", "image/webp", "image/gif"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }
}

impl TryFrom<(Vec<u8>, String)> for Image {
    type Error = &'static str;

    fn try_from((bytes, mime): (Vec<u8>, String)) -> Result<Self, Self::Error> {
        if !Self::allowed().iter().any(|allowed| allowed == &mime) {
            return Err("Unsupported clipboard image type");
        }
        Ok(Self { bytes, mime })
    }
}

/// The largest clipboard image kept, whatever the provider accepts; a
/// provider without its own limit would otherwise keep any size offered.
const MAX_CLIPBOARD_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

pub(super) fn save(image: Image, max_bytes: u64) -> Result<PathBuf, String> {
    let max_bytes = max_bytes.min(MAX_CLIPBOARD_IMAGE_BYTES);
    let (format, extension) = match image.mime.as_str() {
        "image/png" => (image::ImageFormat::Png, ".png"),
        "image/jpeg" => (image::ImageFormat::Jpeg, ".jpg"),
        "image/webp" => (image::ImageFormat::WebP, ".webp"),
        "image/gif" => (image::ImageFormat::Gif, ".gif"),
        _ => return Err("Unsupported clipboard image type".into()),
    };
    if image.bytes.is_empty() || image.bytes.len() as u64 > max_bytes {
        return Err("Clipboard image exceeds this provider's image size limit.".into());
    }
    if image::guess_format(&image.bytes).ok() != Some(format) {
        return Err("Clipboard image data does not match its image type.".into());
    }
    let directory = cache_directory().ok_or("Application cache directory is unavailable.")?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    prune_cache();
    let mut file = tempfile::Builder::new()
        .prefix("paste-")
        .suffix(extension)
        .tempfile_in(directory)
        .map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, &image.bytes).map_err(|error| error.to_string())?;
    file.keep()
        .map(|(_, path)| path)
        .map_err(|error| error.to_string())
}

/// Resolved once: the attachment row asks for every attachment on each redraw.
fn cache_directory() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join("chat-pasted-images"))
        })
        .as_deref()
}

pub(super) fn prune_cache() {
    const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
    static LAST_PRUNE: Mutex<Option<Instant>> = Mutex::new(None);
    let Some(directory) = cache_directory().filter(|directory| directory.is_dir()) else {
        return;
    };
    let Ok(mut last_prune) = LAST_PRUNE.lock() else {
        return;
    };
    if last_prune.is_some_and(|last| last.elapsed() < PRUNE_INTERVAL) {
        return;
    }
    prune_old_images(directory);
    *last_prune = Some(Instant::now());
}

pub(super) fn is_pasted(path: &std::path::Path) -> bool {
    path.parent() == cache_directory()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("paste-"))
}

/// Providers may read paths after a send has returned, including queued
/// prompts. Keep recent images available, then clear old cache files at app
/// startup and on later pastes. Only files created by this feature are touched.
fn prune_old_images(directory: &std::path::Path) {
    const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("paste-"))
        {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= RETENTION)
        {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_and_oversized_clipboard_images() {
        let invalid = Image::try_from((b"plain text".to_vec(), "image/png".into())).unwrap();
        assert!(save(invalid, 100).is_err());
        let image = Image::try_from((b"\x89PNG\r\n\x1a\n".to_vec(), "image/png".into())).unwrap();
        assert!(save(image, 1).is_err());
    }

    #[test]
    fn an_unlimited_provider_still_refuses_huge_clipboard_images() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(MAX_CLIPBOARD_IMAGE_BYTES as usize + 1, 0);
        let image = Image::try_from((bytes, "image/png".into())).unwrap();
        let error = save(image, u64::MAX).unwrap_err();
        assert!(error.contains("size limit"), "{error}");
    }
}
