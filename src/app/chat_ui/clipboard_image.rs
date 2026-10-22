//! Clipboard image data for the native chat composer.

use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use cosmic::iced::clipboard::mime::AllowedMimeTypes;

/// An image offered by the system clipboard. The MIME type decides the file
/// extension; `save` checks the actual bytes before making an attachment.
#[derive(Debug, Clone)]
pub(super) struct Image {
    bytes: std::sync::Arc<[u8]>,
    mime: String,
    oversized: bool,
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
        let oversized = bytes.len() as u64 > MAX_CLIPBOARD_IMAGE_BYTES;
        Ok(Self {
            bytes: if oversized {
                Vec::new().into()
            } else {
                bytes.into()
            },
            mime,
            oversized,
        })
    }
}

/// The largest clipboard image kept, whatever the provider accepts; a
/// provider without its own limit would otherwise keep any size offered.
const MAX_CLIPBOARD_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(test)]
pub(super) fn save(image: Image, max_bytes: u64) -> Result<crate::chat::Attachment, String> {
    save_checked(image, max_bytes, || Ok(()))
}
pub(super) fn save_active(
    image: Image,
    max_bytes: u64,
    ticket: &super::attachment_draft::Ticket,
) -> Result<crate::chat::Attachment, String> {
    save_checked(image, max_bytes, || ticket.check())
}
fn save_checked(
    image: Image,
    max_bytes: u64,
    active: impl Fn() -> Result<(), String>,
) -> Result<crate::chat::Attachment, String> {
    active()?;
    if image.oversized {
        return Err("Clipboard image exceeds the 32 MiB size limit.".into());
    }
    let max_bytes = max_bytes.min(MAX_CLIPBOARD_IMAGE_BYTES);
    let expected = match image.mime.as_str() {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        _ => return Err("Unsupported clipboard image type".into()),
    };
    let actual = super::images::validate_attachment(&image.bytes, max_bytes)?;
    if actual != expected {
        return Err("Clipboard image data does not match its image type.".into());
    }
    let extension = extension(actual)?;
    active()?;
    crate::chat::attachment_assets::stage_validated(
        &image.bytes,
        "Pasted image",
        extension,
        max_bytes,
    )
}

fn extension(format: image::ImageFormat) -> Result<&'static str, String> {
    match format {
        image::ImageFormat::Png => Ok("png"),
        image::ImageFormat::Jpeg => Ok("jpg"),
        image::ImageFormat::Gif => Ok("gif"),
        image::ImageFormat::WebP => Ok("webp"),
        _ => Err("Unsupported attachment image type".into()),
    }
}

/// A user-selected local image is read once, bounded, then validated and
/// copied to immutable device-owned data. Non-image files stay provider files.
pub(super) fn stage_file_active(
    path: PathBuf,
    max_bytes: u64,
    ticket: &super::attachment_draft::Ticket,
) -> Result<crate::chat::Attachment, String> {
    ticket.check()?;
    if crate::device::is_remote() {
        return Err(
            "Attach the image on the conversation's device; local image transfer is unavailable."
                .into(),
        );
    }
    if !crate::chat::is_image_path(&path) {
        return Ok(crate::chat::Attachment::file(path));
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| format!("Cannot open image {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Attachment image is not a regular file.".into());
    }
    let bytes = crate::fs_safety::read_bounded(&mut file, max_bytes.min(MAX_CLIPBOARD_IMAGE_BYTES))
        .map_err(|e| format!("Cannot read image {}: {e}", path.display()))?;
    let actual = super::images::validate_attachment(&bytes, max_bytes)?;
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    ticket.check()?;
    crate::chat::attachment_assets::stage_validated(&bytes, &name, extension(actual)?, max_bytes)
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
    fn cancelled_staging_does_not_read_a_selected_file_or_allocate_a_receipt() {
        let _admission = super::super::attachment_draft::test_admission();
        let (ticket, lease) = super::super::attachment_draft::admit().unwrap();
        drop(lease);
        let error = stage_file_active(
            PathBuf::from("/must-not-read-after-cancellation.png"),
            u64::MAX,
            &ticket,
        )
        .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
    }

    #[test]
    fn cancellation_after_decode_prevents_durable_publication() {
        crate::chat::attachment_assets::with_test_image(|existing| {
            let bytes = std::fs::read(&existing.path).unwrap();
            let image = Image::try_from((bytes, "image/png".into())).unwrap();
            let checks = std::cell::Cell::new(0);
            let error = save_checked(image, u64::MAX, || {
                checks.set(checks.get() + 1);
                if checks.get() == 1 {
                    Ok(())
                } else {
                    Err("cancelled before publication".into())
                }
            })
            .unwrap_err();
            assert!(error.contains("cancelled before publication"));
            assert_eq!(checks.get(), 2);
            assert_eq!(
                crate::chat::attachment_assets::list().unwrap().images.len(),
                1
            );
        });
    }

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
