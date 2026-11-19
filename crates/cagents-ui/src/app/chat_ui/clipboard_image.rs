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
pub struct Image {
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

/// Rejected input is a composer notice; a failed read or save is still an error.
#[derive(Debug, Clone)]
pub enum StagingError {
    Refused(String),
    Failed(String),
}

impl StagingError {
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::Failed(message) => message,
        }
    }
}

impl std::fmt::Display for StagingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

fn storage_error(message: String) -> StagingError {
    if message.starts_with("Image storage is full (2 GiB or 4096 images).")
        || message == "Image storage metadata is full."
        || message == "Image storage exceeds its size limit."
        || message == "Image numbering is exhausted; no identity was reused."
        || message == "Image exceeds this provider's image size limit or has an unsupported type."
    {
        StagingError::Refused(message)
    } else {
        StagingError::Failed(message)
    }
}

#[cfg(test)]
pub(super) fn save(image: Image, max_bytes: u64) -> Result<crate::chat::Attachment, String> {
    save_checked(image, max_bytes, || Ok(())).map_err(|error| error.to_string())
}
pub(super) fn save_active(
    image: Image,
    max_bytes: u64,
    ticket: &super::attachment_draft::Ticket,
) -> Result<crate::chat::Attachment, StagingError> {
    save_checked(image, max_bytes, || ticket.check())
}
fn save_checked(
    image: Image,
    max_bytes: u64,
    active: impl Fn() -> Result<(), String>,
) -> Result<crate::chat::Attachment, StagingError> {
    active().map_err(StagingError::Refused)?;
    if image.oversized {
        return Err(StagingError::Refused(
            "Clipboard image exceeds the 32 MiB size limit.".into(),
        ));
    }
    let max_bytes = max_bytes.min(MAX_CLIPBOARD_IMAGE_BYTES);
    let expected = match image.mime.as_str() {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        _ => {
            return Err(StagingError::Refused(
                "Unsupported clipboard image type".into(),
            ));
        }
    };
    let actual = super::images::validate_attachment(&image.bytes, max_bytes)
        .map_err(StagingError::Refused)?;
    if actual != expected {
        return Err(StagingError::Refused(
            "Clipboard image data does not match its image type.".into(),
        ));
    }
    let extension = extension(actual).map_err(StagingError::Refused)?;
    active().map_err(StagingError::Refused)?;
    crate::chat::attachment_assets::stage_validated(
        &image.bytes,
        "Pasted image",
        extension,
        max_bytes,
    )
    .map_err(storage_error)
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
) -> Result<crate::chat::Attachment, StagingError> {
    ticket.check().map_err(StagingError::Refused)?;
    if crate::device::is_remote() {
        return Err(StagingError::Refused(
            "Attach the image on the conversation's device; local image transfer is unavailable."
                .into(),
        ));
    }
    if !crate::chat::is_image_path(&path) {
        return Ok(crate::chat::Attachment::file(path));
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| StagingError::Failed(format!("Cannot open image {}: {e}", path.display())))?;
    if !file
        .metadata()
        .map_err(|e| StagingError::Failed(e.to_string()))?
        .is_file()
    {
        return Err(StagingError::Refused(
            "Attachment image is not a regular file.".into(),
        ));
    }
    let bytes = crate::fs_safety::read_bounded(&mut file, max_bytes.min(MAX_CLIPBOARD_IMAGE_BYTES))
        .map_err(|e| StagingError::Failed(format!("Cannot read image {}: {e}", path.display())))?;
    let actual =
        super::images::validate_attachment(&bytes, max_bytes).map_err(StagingError::Refused)?;
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    ticket.check().map_err(StagingError::Refused)?;
    crate::chat::attachment_assets::stage_validated(
        &bytes,
        &name,
        extension(actual).map_err(StagingError::Refused)?,
        max_bytes,
    )
    .map_err(storage_error)
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
        assert!(error.to_string().contains("cancelled"), "{error}");
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
            assert!(error.to_string().contains("cancelled before publication"));
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
    fn invalid_clipboard_data_is_a_refusal() {
        let _admission = super::super::attachment_draft::test_admission();
        let (ticket, _lease) = super::super::attachment_draft::admit().unwrap();
        let image = Image::try_from((b"plain text".to_vec(), "image/png".into())).unwrap();
        let error = save_active(image, u64::MAX, &ticket).unwrap_err();
        assert!(matches!(&error, StagingError::Refused(_)), "{error}");
        assert_eq!(error.message(), "The file is not a supported image.");
    }

    #[test]
    fn a_missing_source_image_is_a_failure() {
        let _admission = super::super::attachment_draft::test_admission();
        let (ticket, _lease) = super::super::attachment_draft::admit().unwrap();
        let path = PathBuf::from(format!(
            "/var/tmp/cagents-missing-image-{}.png",
            uuid::Uuid::new_v4()
        ));
        let error = stage_file_active(path.clone(), u64::MAX, &ticket).unwrap_err();
        assert!(matches!(&error, StagingError::Failed(_)), "{error}");
        assert!(
            error
                .message()
                .starts_with(&format!("Cannot open image {}:", path.display())),
            "{error}"
        );
    }

    #[test]
    fn storage_capacity_is_a_refusal_but_storage_failures_remain_errors() {
        for message in [
            "Image storage is full (2 GiB or 4096 images). No images were removed. Run cagents chat image-assets list, then explicitly remove an image you no longer need before attaching another.",
            "Image storage metadata is full.",
            "Image storage exceeds its size limit.",
            "Image numbering is exhausted; no identity was reused.",
            "Image exceeds this provider's image size limit or has an unsupported type.",
        ] {
            let error = storage_error(message.into());
            assert!(matches!(&error, StagingError::Refused(_)), "{error}");
            assert_eq!(error.message(), message);
        }
        for message in [
            "Image storage metadata is damaged; no image identity was changed.",
            "Image storage must be private to this account.",
            "Cannot write image storage: permission denied",
        ] {
            let error = storage_error(message.into());
            assert!(matches!(&error, StagingError::Failed(_)), "{error}");
            assert_eq!(error.message(), message);
        }
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
