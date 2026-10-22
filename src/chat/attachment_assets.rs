//! Durable, device-owned image receipts. Clipboard cache pruning never owns
//! these files. A display number is not an authority to resolve a receipt.
//!
//! Blobs and their registry are private and changed under the existing owned
//! advisory lock. Atomic registry replacement publishes a receipt only after
//! its immutable blob is synced. Drafts and sent/transport-uncertain images
//! remain until explicit scoped cleanup: age alone cannot establish that a
//! persisted draft is unused. Both count toward a hard 2 GiB / 4096-image
//! budget, so failed transports cannot grow storage without bound. No image
//! is silently evicted to admit a new one.
use super::{Attachment, ImageReference};
use crate::fs_safety::{self, Root};
#[cfg(any(feature = "host", feature = "ui"))]
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(any(feature = "ui", all(test, feature = "host")))]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub(crate) const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_STORE_IMAGES: usize = 4096;
const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const REGISTRY: &str = "receipts.json";
const LOCK: &str = "receipts.lock";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u8,
    device: Uuid,
    next_number: u64,
    images: BTreeMap<Uuid, Receipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    reference: ImageReference,
    extension: String,
    bytes: u64,
    created_ms: u64,
    retained: bool,
}
impl Receipt {
    fn leaf(&self) -> String {
        format!("{}.{}", self.reference.id, self.extension)
    }
}

pub(crate) struct Store {
    root: Root,
}
impl Store {
    fn at(path: &Path, create: bool) -> Result<Self, String> {
        if create {
            crate::secure_file::prepare_private_directory(path).map_err(storage_error)?;
        }
        crate::secure_file::reject_symlink_components(path).map_err(storage_error)?;
        crate::secure_file::validate_owned_directory(path).map_err(storage_error)?;
        let root = Root::open(path).map_err(storage_error)?;
        let metadata = root.metadata().map_err(storage_error)?;
        if metadata.mode() & 0o077 != 0 {
            return Err("Image storage must be private to this account.".into());
        }
        Ok(Self { root })
    }
    fn lock(&self) -> Result<crate::instance::OwnedLock, String> {
        use std::os::unix::fs::OpenOptionsExt;
        // Our retained directory descriptor is the only parent. This fixed
        // internal leaf cannot escape or be redirected by renaming the store.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.root.process_path().join(LOCK))
            .map_err(storage_error)?;
        validate_private(&file)?;
        crate::instance::owned_file_lock(file, libc::LOCK_EX).map_err(storage_error)
    }
    fn read_registry(&self) -> Result<Registry, String> {
        let mut file = self.private_file(REGISTRY)?;
        let bytes =
            fs_safety::read_bounded(&mut file, MAX_REGISTRY_BYTES).map_err(storage_error)?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err("Image storage metadata exceeds its limit.".into());
        }
        let registry: Registry = serde_json::from_slice(&bytes).map_err(|_| {
            "Image storage metadata is damaged; no image identity was changed.".to_string()
        })?;
        if registry.version != 1
            || registry.device.is_nil()
            || registry.next_number == 0
            || registry.images.len() > MAX_STORE_IMAGES
        {
            return Err("Image storage metadata is unsupported or damaged.".into());
        }
        let mut numbers = std::collections::HashSet::new();
        let mut bytes = 0u64;
        for (id, receipt) in &registry.images {
            receipt.reference.validate()?;
            if *id != receipt.reference.id
                || receipt.reference.device != registry.device
                || receipt.reference.number >= registry.next_number
                || receipt.reference.extension != receipt.extension
                || !numbers.insert(receipt.reference.number)
                || !matches!(receipt.extension.as_str(), "png" | "jpg" | "gif" | "webp")
                || receipt.bytes == 0
                || receipt.bytes > MAX_IMAGE_BYTES
            {
                return Err("Image storage contains an invalid receipt.".into());
            }
            bytes = bytes
                .checked_add(receipt.bytes)
                .ok_or("Image storage size overflow")?;
        }
        if bytes > MAX_STORE_BYTES {
            return Err("Image storage exceeds its size limit.".into());
        }
        Ok(registry)
    }
    fn private_file(&self, leaf: &str) -> Result<File, String> {
        let file = fs_safety::open_beneath(&self.root, Path::new(leaf)).map_err(storage_error)?;
        validate_private(&file)?;
        Ok(file)
    }
    fn write(&self, leaf: &str, bytes: &[u8], replace: bool) -> Result<(), String> {
        let (mut file, temporary) =
            fs_safety::create_temporary_in(&self.root, 0o600).map_err(storage_error)?;
        let result = (|| {
            file.write_all(bytes).map_err(storage_error)?;
            file.sync_all().map_err(storage_error)?;
            if replace {
                fs_safety::rename_within(&self.root, &temporary, OsStr::new(leaf))
            } else {
                fs_safety::rename_new_within(&self.root, &temporary, OsStr::new(leaf))
            }
            .map_err(storage_error)?;
            fs_safety::sync_directory(&self.root).map_err(storage_error)
        })();
        if result.is_err() {
            let _ = fs_safety::unlink_within(&self.root, &temporary);
        }
        result
    }
    fn write_registry(&self, registry: &Registry) -> Result<(), String> {
        let bytes = serde_json::to_vec(registry).map_err(storage_error)?;
        if bytes.len() as u64 > MAX_REGISTRY_BYTES {
            return Err("Image storage metadata is full.".into());
        }
        self.write(REGISTRY, &bytes, true)
    }
    #[cfg(any(feature = "ui", all(test, feature = "host")))]
    fn initialize(&self) -> Result<Registry, String> {
        match fs_safety::metadata_beneath(&self.root, Path::new(REGISTRY)) {
            Ok(_) => return self.read_registry(),
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(storage_error(error)),
        }
        // Lost metadata must never silently reset numbers/device or turn
        // existing bytes into new, unrelated image identities.
        for entry in std::fs::read_dir(self.root.process_path()).map_err(storage_error)? {
            let name = entry.map_err(storage_error)?.file_name();
            if is_staging_temporary(&name) {
                self.remove_private_temporary(&name)?;
                continue;
            }
            if name != LOCK {
                return Err("Image storage metadata is missing beside existing files; identities were not reset.".into());
            }
        }
        let registry = Registry {
            version: 1,
            device: Uuid::new_v4(),
            next_number: 1,
            images: BTreeMap::new(),
        };
        self.write_registry(&registry)?;
        Ok(registry)
    }
    fn receipt<'a>(
        &self,
        registry: &'a Registry,
        reference: &ImageReference,
    ) -> Result<&'a Receipt, String> {
        reference.validate()?;
        if reference.device != registry.device {
            return Err(format!(
                "{} belongs to another device. Attach the image on this conversation's device.",
                reference.label()
            ));
        }
        registry
            .images
            .get(&reference.id)
            .filter(|receipt| receipt.reference == *reference)
            .ok_or_else(|| {
                format!(
                    "{} is unavailable. Attach the image again.",
                    reference.label()
                )
            })
    }
    fn attachment(&self, reference: &ImageReference) -> Result<Attachment, String> {
        let registry = self.read_registry()?;
        let receipt = self.receipt(&registry, reference)?;
        let attachment = Attachment {
            path: self.root.path().join(receipt.leaf()),
            reference: Some(reference.clone()),
        };
        self.validate_with(&registry, &attachment)?;
        Ok(attachment)
    }
    fn read(&self, reference: &ImageReference, limit: usize) -> Result<Vec<u8>, String> {
        let registry = self.read_registry()?;
        let receipt = self.receipt(&registry, reference)?;
        let limit = limit.min(MAX_IMAGE_BYTES as usize);
        if receipt.bytes > limit as u64 {
            return Err(format!(
                "{} exceeds the {} KiB image preview limit on its device.",
                reference.label(),
                limit / 1024
            ));
        }
        let mut file = self.private_file(&receipt.leaf())?;
        let bytes = fs_safety::read_bounded(&mut file, receipt.bytes).map_err(storage_error)?;
        if bytes.len() as u64 != receipt.bytes || digest(&bytes) != reference.sha256 {
            return Err(format!(
                "{} has changed or is missing. Attach the image again.",
                reference.label()
            ));
        }
        Ok(bytes)
    }

    fn validate_with(&self, registry: &Registry, attachment: &Attachment) -> Result<(), String> {
        let Some(reference) = &attachment.reference else {
            return Ok(());
        };
        let receipt = self.receipt(registry, reference)?;
        if attachment.path != self.root.path().join(receipt.leaf()) {
            return Err(format!(
                "{} does not match its owned image. Attach the image again.",
                reference.label()
            ));
        }
        let mut file = self.private_file(&receipt.leaf())?;
        let bytes = fs_safety::read_bounded(&mut file, receipt.bytes).map_err(storage_error)?;
        if bytes.len() as u64 != receipt.bytes || digest(&bytes) != reference.sha256 {
            return Err(format!(
                "{} has changed or is missing. Attach the image again.",
                reference.label()
            ));
        }
        Ok(())
    }
    pub(crate) fn retain(&self, attachment: &Attachment) -> Result<(), String> {
        let Some(reference) = &attachment.reference else {
            return Ok(());
        };
        let _lock = self.lock()?;
        let mut registry = self.read_registry()?;
        let receipt = self.receipt(&registry, reference)?;
        // A displaying client may carry another machine's path spelling.
        // Only this backend's UUID receipt chooses the file to validate/retain.
        let owned = Attachment {
            path: self.root.path().join(receipt.leaf()),
            reference: Some(reference.clone()),
        };
        self.validate_with(&registry, &owned)?;
        let receipt = registry
            .images
            .get_mut(&reference.id)
            .ok_or("Image receipt disappeared")?;
        if !receipt.retained {
            receipt.retained = true;
            self.write_registry(&registry)?;
        }
        Ok(())
    }
    #[cfg(any(feature = "host", feature = "ui"))]
    fn restore(&self, reference: &ImageReference, source: &str) -> Option<ImageReference> {
        let registry = self.read_registry().ok()?;
        let receipt = self.receipt(&registry, reference).ok()?;
        if let Some(data) = source.strip_prefix("data:image/") {
            let (kind, data) = data.split_once(";base64,")?;
            if !matches!(kind, "png" | "jpeg" | "gif" | "webp")
                || data.len() != (receipt.bytes as usize).div_ceil(3) * 4
            {
                return None;
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?;
            if bytes.len() as u64 != receipt.bytes || digest(&bytes) != reference.sha256 {
                return None;
            }
        } else {
            let attachment = Attachment {
                path: source.into(),
                reference: Some(reference.clone()),
            };
            self.validate_with(&registry, &attachment).ok()?;
        }
        Some(reference.clone())
    }

    /// Caller has validated real image decoding under UI's shared budgets.
    #[cfg(any(feature = "ui", all(test, feature = "host")))]
    pub(crate) fn stage_validated(
        &self,
        bytes: &[u8],
        name: &str,
        extension: &str,
        max_bytes: u64,
    ) -> Result<Attachment, String> {
        if bytes.is_empty()
            || bytes.len() as u64 > max_bytes.min(MAX_IMAGE_BYTES)
            || !matches!(extension, "png" | "jpg" | "gif" | "webp")
        {
            return Err(
                "Image exceeds this provider's image size limit or has an unsupported type.".into(),
            );
        }
        let _lock = self.lock()?;
        let mut registry = self.initialize()?;
        let now = now_ms()?;
        self.remove_orphans(&registry)?;
        let total = registry.images.values().map(|r| r.bytes).sum::<u64>();
        if registry.images.len() >= MAX_STORE_IMAGES || total + bytes.len() as u64 > MAX_STORE_BYTES
        {
            return Err("Image storage is full (2 GiB or 4096 images). No images were removed. Run cagents chat image-assets list, then explicitly remove an image you no longer need before attaching another.".into());
        }
        let number = registry.next_number;
        registry.next_number = number
            .checked_add(1)
            .ok_or("Image numbering is exhausted; no identity was reused.")?;
        let name = name
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect::<String>();
        let reference = ImageReference {
            id: Uuid::new_v4(),
            number,
            device: registry.device,
            name,
            sha256: digest(bytes),
            extension: extension.into(),
        };
        reference.validate()?;
        let receipt = Receipt {
            reference: reference.clone(),
            extension: extension.into(),
            bytes: bytes.len() as u64,
            created_ms: now,
            retained: false,
        };
        let leaf = receipt.leaf();
        self.write(&leaf, bytes, false)?;
        registry.images.insert(reference.id, receipt);
        // Do not delete on uncertain registry fsync failure: the new receipt
        // may already be visible to another process. Orphans are reclaimed at
        // the next locked stage only when no current receipt owns them.
        self.write_registry(&registry)?;
        Ok(Attachment {
            path: self.root.path().join(leaf),
            reference: Some(reference),
        })
    }
    #[cfg(any(feature = "ui", all(test, feature = "host")))]
    fn remove_private_temporary(&self, name: &OsStr) -> Result<(), String> {
        self.private_file(name.to_str().ok_or("Invalid temporary image filename")?)?;
        fs_safety::unlink_within(&self.root, name).map_err(storage_error)
    }
    #[cfg(any(feature = "ui", all(test, feature = "host")))]
    fn remove_orphans(&self, registry: &Registry) -> Result<(), String> {
        let mut unrecognized = false;
        for (count, entry) in std::fs::read_dir(self.root.process_path())
            .map_err(storage_error)?
            .enumerate()
        {
            if count > MAX_STORE_IMAGES + 128 {
                return Err("Image storage has too many unrecognized files.".into());
            }
            let name = entry.map_err(storage_error)?.file_name();
            if is_staging_temporary(&name) {
                self.remove_private_temporary(&name)?;
                continue;
            }
            if name == REGISTRY || name == LOCK {
                continue;
            }
            let path = Path::new(&name);
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                unrecognized = true;
                continue;
            };
            if registry
                .images
                .get(&id)
                .is_some_and(|receipt| name.as_os_str() == OsStr::new(&receipt.leaf()))
            {
                continue;
            }
            if !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("png" | "jpg" | "gif" | "webp")
            ) {
                unrecognized = true;
                continue;
            }
            let leaf = name.to_str().ok_or("Invalid image storage filename")?;
            // Refuse links/nonprivate files rather than deleting arbitrary
            // directory content. This namespace owns only its UUID blobs.
            self.private_file(leaf)?;
            fs_safety::unlink_within(&self.root, &name).map_err(storage_error)?;
        }
        if unrecognized {
            return Err(format!(
                "Image storage contains unrecognized files in {}. No unknown files were removed; inspect that directory before attaching another image.",
                self.root.path().display()
            ));
        }
        Ok(())
    }
}
/// Exact create_temporary_in naming contract: 128 random bits as lowercase
/// hex. Recognition is limited to our private store while its writer lock is
/// held, followed by nofollow/singly-linked/private validation before unlink.
#[cfg(any(feature = "ui", all(test, feature = "host")))]
fn is_staging_temporary(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|s| s.strip_prefix(".cagents-tmp-"))
        .is_some_and(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}
fn validate_private(file: &File) -> Result<(), String> {
    let metadata = file.metadata().map_err(storage_error)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err("Image storage requires singly linked private regular files.".into());
    }
    Ok(())
}
/// Safe inventory for explicit local user management; source filenames and
/// private paths are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetSummary {
    pub id: Uuid,
    pub number: u64,
    pub sha256: String,
    pub bytes: u64,
    pub retained: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub device: Option<Uuid>,
    pub images: Vec<AssetSummary>,
    pub bytes: u64,
    pub max_bytes: u64,
    pub max_images: usize,
}

pub fn list() -> Result<Inventory, String> {
    let path = store_path()?;
    crate::secure_file::reject_symlink_components(&path).map_err(storage_error)?;
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Inventory {
                device: None,
                images: Vec::new(),
                bytes: 0,
                max_bytes: MAX_STORE_BYTES,
                max_images: MAX_STORE_IMAGES,
            });
        }
        Err(error) => return Err(storage_error(error)),
        Ok(_) => {}
    }
    let registry = Store::at(&path, false)?.read_registry()?;
    Ok(Inventory {
        device: Some(registry.device),
        bytes: registry.images.values().map(|r| r.bytes).sum(),
        max_bytes: MAX_STORE_BYTES,
        max_images: MAX_STORE_IMAGES,
        images: registry
            .images
            .values()
            .map(|r| AssetSummary {
                id: r.reference.id,
                number: r.reference.number,
                sha256: r.reference.sha256.clone(),
                bytes: r.bytes,
                retained: r.retained,
            })
            .collect(),
    })
}

/// This is an explicit physical-device user-management operation. Its caller
/// must reject hosted and remote-selected authority before opening storage.
/// UUID plus expected immutable digest is a receipt CAS, never a filesystem
/// path. Removal can make draft/history images unavailable, hence the opt-in.
pub fn remove(
    id: Uuid,
    expected_sha256: &str,
    acknowledge_history_loss: bool,
) -> Result<(), String> {
    if !acknowledge_history_loss {
        return Err("Removing an image can make draft and history images unavailable; acknowledge this loss explicitly.".into());
    }
    if id.is_nil()
        || expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(
            "An exact image UUID and lowercase SHA-256 receipt digest are required.".into(),
        );
    }
    let store = store(false)?;
    let _lock = store.lock()?;
    let mut registry = store.read_registry()?;
    let receipt = registry
        .images
        .get(&id)
        .ok_or("No image receipt with that UUID exists on this physical device.")?;
    if receipt.reference.sha256 != expected_sha256 {
        return Err(
            "The image receipt changed or its expected digest does not match; nothing was removed."
                .into(),
        );
    }
    let leaf = receipt.leaf();
    registry.images.remove(&id);
    store.write_registry(&registry)?;
    // Authority is removed durably before bytes. A missing/damaged blob must
    // not prevent users releasing its exact receipt/quota. No symlink follows.
    match fs_safety::unlink_within(&store.root, OsStr::new(&leaf)) {
        Ok(()) => fs_safety::sync_directory(&store.root).map_err(storage_error)?,
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(storage_error(error)),
    }
    Ok(())
}

fn storage_error(error: impl std::fmt::Display) -> String {
    format!("Image storage: {error}")
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[cfg(any(feature = "ui", all(test, feature = "host")))]
fn now_ms() -> Result<u64, String> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(storage_error)?
            .as_millis(),
    )
    .map_err(storage_error)
}
#[cfg(test)]
thread_local! { static TEST_DIRECTORY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
fn store_path() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_DIRECTORY.with(|path| path.borrow().clone()) {
        return Ok(path);
    }
    Ok(crate::instance::project_dirs()
        .ok_or("Application image storage is unavailable")?
        .data_dir()
        .join("chat-image-assets"))
}
fn store(create: bool) -> Result<Store, String> {
    Store::at(&store_path()?, create)
}

pub fn validate(attachment: &Attachment) -> Result<(), String> {
    let Some(reference) = &attachment.reference else {
        return Ok(());
    };
    let store = store(false).map_err(|error| format!("{} cannot be verified on this device. Attach the image on the conversation's device. {error}", reference.label()))?;
    store.validate_with(&store.read_registry()?, attachment)
}

pub fn retain(attachment: &Attachment) -> Result<(), String> {
    if attachment.reference.is_none() {
        return Ok(());
    }
    store(false)?.retain(attachment)
}
pub fn attachment(reference: &ImageReference) -> Result<Attachment, String> {
    store(false)?.attachment(reference)
}

/// Resolve only a managed UUID receipt on the receiving physical backend.
/// The path supplied by a displaying client is a hint, never file authority.
/// Legacy path-only files keep their existing provider-specific behavior.
pub fn resolve(attachment: &Attachment) -> Result<Attachment, String> {
    match &attachment.reference {
        Some(reference) => self::attachment(reference),
        None => Ok(attachment.clone()),
    }
}

/// Read an exact immutable backend receipt within the caller's transport cap.
/// No caller-supplied path is accepted, and no missing/foreign receipt falls
/// back to a same-named file on the displaying computer.
pub fn read(reference: &ImageReference, limit: usize) -> Result<Vec<u8>, String> {
    store(false)?.read(reference, limit)
}
#[cfg(any(feature = "host", feature = "ui"))]
pub fn restore_reference(reference: &ImageReference, source: &str) -> Option<ImageReference> {
    store(false).ok()?.restore(reference, source)
}
#[cfg(feature = "ui")]
pub(crate) fn stage_validated(
    bytes: &[u8],
    name: &str,
    extension: &str,
    max_bytes: u64,
) -> Result<Attachment, String> {
    store(true)?.stage_validated(bytes, name, extension, max_bytes)
}

/// Thread-local, private real storage. Provider regressions exercise the same
/// public APIs without changing XDG variables or another test's device.
#[cfg(all(test, any(feature = "ui", feature = "host")))]
pub(crate) fn with_test_image<T>(test: impl FnOnce(Attachment) -> T) -> T {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("assets");
    let store = Store::at(&path, true).unwrap();
    let bytes = fixture_bytes("first image");
    let attachment = store
        .stage_validated(&bytes, "test image.png", "png", MAX_IMAGE_BYTES)
        .unwrap();
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_DIRECTORY.with(|path| *path.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_DIRECTORY.with(|slot| slot.replace(Some(path))));
    test(attachment)
}

#[cfg(all(test, any(feature = "ui", feature = "host")))]
pub(crate) fn test_image(name: &str) -> Attachment {
    let bytes = fixture_bytes(name);
    store(false)
        .unwrap()
        .stage_validated(&bytes, name, "png", MAX_IMAGE_BYTES)
        .unwrap()
}

#[cfg(all(test, any(feature = "ui", feature = "host")))]
fn fixture_bytes(name: &str) -> Vec<u8> {
    use image::ImageEncoder;
    let hash = Sha256::digest(name.as_bytes());
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &[hash[0], hash[1], hash[2], 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    bytes
}

#[cfg(all(test, any(feature = "ui", feature = "host")))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn backend_resolution_ignores_display_paths_and_never_rebinds_foreign_receipts() {
        with_test_image(|backend| {
            let display = tempfile::tempdir().unwrap();
            let coincident = display.path().join(backend.path.file_name().unwrap());
            std::fs::write(&coincident, b"different display pixels").unwrap();
            let hinted = Attachment {
                path: coincident.clone(),
                reference: backend.reference.clone(),
            };
            assert_eq!(resolve(&hinted).unwrap(), backend);
            retain(&hinted).unwrap();
            assert!(
                list()
                    .unwrap()
                    .images
                    .iter()
                    .find(|row| row.id == backend.reference.as_ref().unwrap().id)
                    .unwrap()
                    .retained
            );
            assert_eq!(
                std::fs::read(coincident).unwrap(),
                b"different display pixels"
            );
            let expected = std::fs::read(&backend.path).unwrap();
            assert_eq!(
                read(backend.reference.as_ref().unwrap(), expected.len()).unwrap(),
                expected
            );
            assert_eq!(
                resolve(&Attachment::file("relative legacy.png")).unwrap(),
                Attachment::file("relative legacy.png")
            );
            with_test_image(|other_device| {
                assert_ne!(
                    backend.reference.as_ref().unwrap().device,
                    other_device.reference.as_ref().unwrap().device
                );
                let foreign = Attachment {
                    path: other_device.path.clone(),
                    reference: backend.reference.clone(),
                };
                assert!(resolve(&foreign).unwrap_err().contains("another device"));
                assert!(retain(&foreign).unwrap_err().contains("another device"));
                assert!(
                    read(
                        backend.reference.as_ref().unwrap(),
                        MAX_IMAGE_BYTES as usize
                    )
                    .unwrap_err()
                    .contains("another device")
                );
            });
        });
    }

    #[test]
    fn receipt_reads_check_declared_cap_before_file_access_and_verify_exact_bytes() {
        with_test_image(|image| {
            let reference = image.reference.as_ref().unwrap();
            let bytes = std::fs::read(&image.path).unwrap();
            assert!(
                read(reference, bytes.len() - 1)
                    .unwrap_err()
                    .contains("preview limit")
            );
            std::fs::write(&image.path, vec![0; bytes.len()]).unwrap();
            assert!(
                read(reference, bytes.len())
                    .unwrap_err()
                    .contains("changed or is missing")
            );
            std::fs::remove_file(&image.path).unwrap();
            assert!(
                read(reference, bytes.len() - 1)
                    .unwrap_err()
                    .contains("preview limit"),
                "cap precedes opening the blob"
            );
            assert!(
                read(reference, bytes.len()).is_err(),
                "missing bytes never fall back to another path"
            );
        });
    }

    #[test]
    fn immutable_receipt_roundtrip_rejects_changed_pixels_paths_and_devices() {
        with_test_image(|image| {
            validate(&image).unwrap();
            let reference = image.reference.as_ref().unwrap();
            assert_eq!(attachment(reference).unwrap(), image);
            assert_eq!(
                restore_reference(reference, image.path.to_str().unwrap()),
                Some(reference.clone())
            );
            let bytes = std::fs::read(&image.path).unwrap();
            let inline = format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            );
            assert_eq!(
                restore_reference(reference, &inline),
                Some(reference.clone())
            );
            assert!(restore_reference(reference, "data:image/png;base64,AA==").is_none());
            let mut changed = image.clone();
            changed.path = "/must-not-open-this.png".into();
            assert!(validate(&changed).unwrap_err().contains("does not match"));
            changed.reference.as_mut().unwrap().device = Uuid::new_v4();
            assert!(validate(&changed).unwrap_err().contains("another device"));
            let mut forged = reference.clone();
            forged.id = Uuid::new_v4();
            assert!(attachment(&forged).is_err());
            std::fs::write(&image.path, b"changed pixels").unwrap();
            assert!(validate(&image).is_err());
            assert!(restore_reference(reference, image.path.to_str().unwrap()).is_none());
        });
    }

    #[test]
    fn old_persisted_drafts_and_retained_history_survive_new_insertions() {
        with_test_image(|sent| {
            retain(&sent).unwrap();
            let draft = test_image("persisted draft.png");
            let store = store(false).unwrap();
            let mut registry = store.read_registry().unwrap();
            for receipt in registry.images.values_mut() {
                receipt.created_ms = 0;
            }
            store.write_registry(&registry).unwrap();
            let new = test_image("next.png");
            assert!(
                new.reference.as_ref().unwrap().number > draft.reference.as_ref().unwrap().number
            );
            validate(&sent).unwrap();
            validate(&draft).unwrap();
            assert!(sent.path.is_file());
            assert!(draft.path.is_file());
        });
    }

    #[test]
    fn private_storage_refuses_symlinks_hardlinks_and_public_files() {
        with_test_image(|image| {
            let store = store(false).unwrap();
            let registry = store.read_registry().unwrap();
            let receipt = registry.images.values().next().unwrap();
            let other = image.path.with_extension("other");
            std::fs::hard_link(&image.path, &other).unwrap();
            assert!(validate(&image).unwrap_err().contains("singly linked"));
            std::fs::remove_file(&other).unwrap();
            std::fs::set_permissions(&image.path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(validate(&image).unwrap_err().contains("private"));
            std::fs::set_permissions(&image.path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::rename(&image.path, &other).unwrap();
            symlink(&other, &image.path).unwrap();
            assert!(store.private_file(&receipt.leaf()).is_err());
            assert!(validate(&image).is_err());
            assert_eq!(std::fs::read(&other).unwrap().len() as u64, receipt.bytes);
        });
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(Store::at(&link, true).is_err());
    }

    #[test]
    fn interrupted_stages_reclaim_exact_private_temps_and_orphans_but_not_unknown_files_or_links() {
        with_test_image(|image| {
            let store = store(false).unwrap();
            let bytes = std::fs::read(&image.path).unwrap();
            let (mut temporary, temporary_name) =
                fs_safety::create_temporary_in(&store.root, 0o600).unwrap();
            temporary.write_all(&bytes).unwrap();
            temporary.sync_all().unwrap();
            drop(temporary);
            let orphan = format!("{}.png", Uuid::new_v4());
            store.write(&orphan, &bytes, false).unwrap();
            let unknown = store.root.path().join("keep-user-file");
            std::fs::write(&unknown, b"do not remove").unwrap();
            assert!(
                store
                    .stage_validated(
                        &bytes,
                        "refuse unrecognized storage",
                        "png",
                        MAX_IMAGE_BYTES
                    )
                    .unwrap_err()
                    .contains("unrecognized")
            );
            assert!(!store.root.path().join(&temporary_name).exists());
            assert!(!store.root.path().join(&orphan).exists());
            assert_eq!(std::fs::read(&unknown).unwrap(), b"do not remove");
            std::fs::remove_file(&unknown).unwrap();
            let next = test_image("after interrupted stage");
            validate(&image).unwrap();
            validate(&next).unwrap();
            let linked_temporary = store
                .root
                .path()
                .join(".cagents-tmp-0123456789abcdef0123456789abcdef");
            symlink(&image.path, &linked_temporary).unwrap();
            assert!(
                store
                    .stage_validated(&bytes, "must refuse link", "png", MAX_IMAGE_BYTES)
                    .is_err()
            );
            assert!(
                std::fs::symlink_metadata(&linked_temporary)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            validate(&image).unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(&directory.path().join("assets"), true).unwrap();
        let (mut temporary, temporary_name) =
            fs_safety::create_temporary_in(&store.root, 0o600).unwrap();
        temporary
            .write_all(b"interrupted first registry write")
            .unwrap();
        drop(temporary);
        let bytes = fixture_bytes("first after crash");
        let image = store
            .stage_validated(&bytes, "first", "png", MAX_IMAGE_BYTES)
            .unwrap();
        assert_eq!(image.reference.unwrap().number, 1);
        assert!(!store.root.path().join(temporary_name).exists());
    }

    #[test]
    fn explicit_cleanup_requires_exact_receipt_and_history_loss_opt_in_without_rebinding() {
        with_test_image(|image| {
            let reference = image.reference.clone().unwrap();
            let bytes = std::fs::read(&image.path).unwrap();
            let inline = format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            );
            retain(&image).unwrap();
            let before = list().unwrap();
            assert!(before.images[0].retained);
            assert!(remove(reference.id, &reference.sha256, false).is_err());
            assert!(remove(reference.id, &"b".repeat(64), true).is_err());
            assert!(remove(Uuid::new_v4(), &reference.sha256, true).is_err());
            validate(&image).unwrap();
            remove(reference.id, &reference.sha256, true).unwrap();
            assert!(!image.path.exists());
            assert!(attachment(&reference).is_err());
            assert!(
                restore_reference(&reference, &inline).is_none(),
                "old UUID authority stays removed even when provider history still owns original bytes"
            );
            let next = test_image("after explicit removal.png");
            let next_ref = next.reference.as_ref().unwrap();
            assert_eq!(next_ref.device, reference.device);
            assert!(next_ref.number > reference.number);
            assert_ne!(next_ref.id, reference.id);
            assert_eq!(list().unwrap().device, before.device);
            assert!(restore_reference(&reference, next.path.to_str().unwrap()).is_none());
        });
    }

    #[test]
    fn counter_overflow_and_lost_metadata_do_not_reset_identity() {
        with_test_image(|image| {
            let store = store(false).unwrap();
            let mut registry = store.read_registry().unwrap();
            registry.next_number = u64::MAX;
            store.write_registry(&registry).unwrap();
            let bytes = std::fs::read(&image.path).unwrap();
            assert!(
                store
                    .stage_validated(&bytes, "new", "png", MAX_IMAGE_BYTES)
                    .unwrap_err()
                    .contains("exhausted")
            );
            assert_eq!(store.read_registry().unwrap().next_number, u64::MAX);
            std::fs::remove_file(store.root.path().join(REGISTRY)).unwrap();
            assert!(
                store
                    .stage_validated(&bytes, "new", "png", MAX_IMAGE_BYTES)
                    .unwrap_err()
                    .contains("missing")
            );
            assert!(image.path.exists());
        });
    }
}
