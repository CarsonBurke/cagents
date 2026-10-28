//! Image handles are shared with the transcript's cache. Opening the gallery
//! does not decode base64, read a file, or copy a pixel buffer.
use super::*;
mod canvas;

// Keep the pixel lease in the decoded buffer itself: cached turns, galleries,
// detached widget trees and renderer host buffers all clone it. Counting cache
// entries alone would release their budget while those clones still exist.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const ENCODED_BUDGET: usize = 64 * 1024 * 1024;
// Retained RGBA buffers share this budget, including renderer host clones. GPU
// atlas allocations follow iced's separate lifecycle. Decoder-internal scratch
// obeys its best-effort limit; this does not cap total process/renderer memory.
const PIXEL_BUDGET: usize = 128 * 1024 * 1024;
const DECODE_BUDGET: usize = 512 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = MAX_PIXELS * 16;
const RECENT_IMAGES: usize = 8;

pub(super) struct Budget {
    encoded: Arc<tokio::sync::Semaphore>,
    pixels: Arc<tokio::sync::Semaphore>,
    decode: Arc<tokio::sync::Semaphore>,
}
impl Budget {
    pub(super) fn new(encoded: usize, pixels: usize) -> Self {
        Self {
            encoded: Arc::new(tokio::sync::Semaphore::new(encoded)),
            pixels: Arc::new(tokio::sync::Semaphore::new(pixels)),
            decode: Arc::new(tokio::sync::Semaphore::new(DECODE_BUDGET)),
        }
    }
    #[cfg(test)]
    pub(super) fn block_decoder(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.decode
            .clone()
            .try_acquire_many_owned(DECODE_BUDGET as u32)
            .unwrap()
    }
}
fn budget() -> &'static Budget {
    static BUDGET: std::sync::OnceLock<Budget> = std::sync::OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(ENCODED_BUDGET, PIXEL_BUDGET))
}
struct Pixels {
    bytes: Box<[u8]>,
    _pixels: tokio::sync::OwnedSemaphorePermit,
}
/// The exact resource that prevented admission. Waiting does not occupy an
/// image worker, and only real permit release (including last widget/gallery
/// owners) can satisfy it. Semaphore admission remains the memory authority.
#[derive(Clone)]
pub(in crate::app) struct Pressure {
    resource: Arc<tokio::sync::Semaphore>,
    permits: u32,
    retained: bool,
}
impl Pressure {
    pub(super) async fn available(&self) {
        let permit = self.resource.clone().acquire_many_owned(self.permits).await;
        drop(permit);
    }
}
impl PartialEq for Pressure {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.resource, &other.resource)
            && self.permits == other.permits
            && self.retained == other.retained
    }
}
impl Eq for Pressure {}
impl std::hash::Hash for Pressure {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.resource).hash(state);
        self.permits.hash(state);
        self.retained.hash(state);
    }
}
fn reserve(
    resource: &Arc<tokio::sync::Semaphore>,
    permits: u32,
    retained: bool,
) -> Result<tokio::sync::OwnedSemaphorePermit, Loaded> {
    resource
        .clone()
        .try_acquire_many_owned(permits)
        .map_err(|_| {
            Loaded::Deferred(Pressure {
                resource: resource.clone(),
                permits,
                retained,
            })
        })
}
impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
#[derive(Clone)]
pub(in crate::app) enum Loaded {
    Ready(iced::widget::image::Handle),
    Unavailable,
    Failed(String),
    Deferred(Pressure),
}
pub(super) fn bounded_handle(bytes: Vec<u8>, budget: &Budget) -> Loaded {
    bounded_handle_size(bytes, budget, None)
}
fn bounded_handle_size(bytes: Vec<u8>, budget: &Budget, edge: Option<u32>) -> Loaded {
    decode_handle(&bytes, budget, edge, chat::MAX_INLINE_IMAGE_BYTES)
}

#[derive(Clone, Copy)]
enum DecodeAdmission {
    Defer,
    Wait,
}
struct DecoderInput<'a> {
    bytes: &'a [u8],
    orientation_override: Option<image::metadata::Orientation>,
    ico_dimensions: Option<(u16, u16)>,
}
fn webp_chunk(bytes: &[u8], offset: usize) -> Option<(&[u8], &[u8], usize)> {
    let header_end = offset.checked_add(8)?;
    let header = bytes.get(offset..header_end)?;
    let length = usize::try_from(u32::from_le_bytes(header[4..8].try_into().ok()?)).ok()?;
    let payload_end = header_end.checked_add(length)?;
    let next = payload_end.checked_add(length & 1)?;
    if next > bytes.len() {
        return None;
    }
    Some((&header[..4], &bytes[header_end..payload_end], next))
}
fn webp_vp8_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..10)?;
    if header[0] & 1 != 0 || &header[3..6] != b"\x9d\x01\x2a" {
        return None;
    }
    Some((
        u32::from(u16::from_le_bytes([header[6], header[7]]) & 0x3fff),
        u32::from(u16::from_le_bytes([header[8], header[9]]) & 0x3fff),
    ))
}
fn webp_frame_matches_vp8(frame: &[u8]) -> Option<()> {
    let header = frame.get(..16)?;
    let dimension = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) + 1;
    let expected = (dimension(&header[6..9]), dimension(&header[9..12]));
    let (kind, payload, next) = webp_chunk(frame, 16)?;
    let vp8 = match kind {
        b"VP8 " => payload,
        b"VP8L" => return Some(()),
        b"ALPH" => {
            let (kind, payload, _) = webp_chunk(frame, next)?;
            if kind != b"VP8 " {
                return None;
            }
            payload
        }
        _ => return None,
    };
    (webp_vp8_dimensions(vp8)? == expected).then_some(())
}
fn bounded_decoder_input(bytes: &[u8]) -> Option<DecoderInput<'_>> {
    if bytes.get(..4) == Some(b"\0\0\x01\0") {
        return bounded_ico_input(bytes);
    }
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Some(DecoderInput {
            bytes,
            orientation_override: None,
            ico_dimensions: None,
        });
    }
    // The pinned WebP decoder records declared metadata ranges before proving
    // they exist. Bound every top-level chunk, including its odd-byte padding,
    // without allocating or letting a seek adopt bytes beyond this container.
    let length = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().ok()?)).ok()?;
    let end = length.checked_add(8)?;
    if end < 12 {
        return None;
    }
    let container = bytes.get(..end)?;
    let mut offset = 12usize;
    let mut exif = None;
    let mut extended = None;
    let mut vp8 = None;
    let mut frame = None;
    while offset < end {
        let (kind, payload, next) = webp_chunk(container, offset)?;
        if offset == 12 && kind == b"VP8X" {
            let header = payload.get(..10)?;
            let dimension =
                |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) + 1;
            extended = Some((
                header[0] & 0x02 != 0,
                (dimension(&header[4..7]), dimension(&header[7..10])),
            ));
        }
        match kind {
            b"EXIF" if exif.is_none() => exif = Some(payload),
            b"VP8 " if vp8.is_none() => vp8 = Some(payload),
            b"ANMF" if frame.is_none() => frame = Some(payload),
            _ => {}
        }
        offset = next;
    }
    if let Some((animated, dimensions)) = extended {
        // The pinned VP8 decoder allocates YUV from its own keyframe header
        // before comparing against the admitted canvas/frame. Check only the
        // first image/frame actually decoded, using its real bounded payload.
        if animated {
            webp_frame_matches_vp8(frame?)?;
        } else if let Some(vp8) = vp8
            && webp_vp8_dimensions(vp8)? != dimensions
        {
            return None;
        }
    }
    // Never ask the inner WebP decoder to allocate EXIF from its range map:
    // its animation-header scan can also manufacture a range from frame data.
    // Only the first actual top-level EXIF payload supplies orientation.
    Some(DecoderInput {
        bytes: container,
        orientation_override: Some(
            exif.and_then(image::metadata::Orientation::from_exif_chunk)
                .unwrap_or(image::metadata::Orientation::NoTransforms),
        ),
        ico_dimensions: None,
    })
}
fn bounded_ico_input(bytes: &[u8]) -> Option<DecoderInput<'_>> {
    let count = usize::from(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?));
    if count == 0 {
        return None;
    }
    let directory_end = 6usize.checked_add(count.checked_mul(16)?)?;
    let directory = bytes.get(6..directory_end)?;
    let dimension = |value: u8| if value == 0 { 256 } else { u16::from(value) };
    let score = |entry: &[u8]| {
        (
            u16::from_le_bytes([entry[6], entry[7]]),
            u32::from(dimension(entry[0])) * u32::from(dimension(entry[1])),
        )
    };
    // Match image 0.25.10: seed the last entry, then replace only on a
    // strictly greater score while visiting earlier entries in file order.
    let mut selected = &directory[directory.len() - 16..];
    for entry in directory.chunks_exact(16) {
        if u16::from_le_bytes([entry[4], entry[5]]) > 256
            || u16::from_le_bytes([entry[6], entry[7]]) > 256
        {
            return None;
        }
        if score(entry) > score(selected) {
            selected = entry;
        }
    }
    let length = usize::try_from(u32::from_le_bytes(selected[8..12].try_into().ok()?)).ok()?;
    let start = usize::try_from(u32::from_le_bytes(selected[12..16].try_into().ok()?)).ok()?;
    let image = bytes.get(start..start.checked_add(length)?)?;
    let is_png = image.get(..8)? == b"\x89PNG\r\n\x1a\n";
    Some(DecoderInput {
        // PNG must receive limits before read_info inflates metadata. Keep
        // DIBs on the existing ICO decoder, including its AND-mask behavior.
        bytes: if is_png { image } else { bytes },
        orientation_override: is_png.then_some(image::metadata::Orientation::NoTransforms),
        ico_dimensions: is_png.then_some((dimension(selected[0]), dimension(selected[1]))),
    })
}
fn decoder_with_limits<'a>(
    input: &DecoderInput<'a>,
    max_alloc: u64,
) -> Option<impl image::ImageDecoder + 'a> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(input.bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    // Each axis of a positive image within the existing pixel-product limit
    // fits this ceiling, without an unadmitted dimension-only constructor.
    limits.max_image_width = Some(MAX_PIXELS as u32);
    limits.max_image_height = Some(MAX_PIXELS as u32);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    let decoder = reader.into_decoder().ok()?;
    if let Some((width, height)) = input.ico_dimensions {
        let (actual_width, actual_height) = decoder.dimensions();
        // Preserve the ICO wrapper's PNG-specific checks and its treatment
        // of a zero directory dimension as 256, including larger PNGs.
        if u32::from(width) != actual_width.min(256)
            || u32::from(height) != actual_height.min(256)
            || decoder.color_type() != image::ColorType::Rgba8
        {
            return None;
        }
    }
    Some(decoder)
}
fn decode_handle(bytes: &[u8], budget: &Budget, edge: Option<u32>, max_bytes: usize) -> Loaded {
    decode_handle_with_admission(bytes, budget, edge, max_bytes, DecodeAdmission::Defer)
}
fn decode_handle_with_admission(
    bytes: &[u8],
    budget: &Budget,
    edge: Option<u32>,
    max_bytes: usize,
    admission: DecodeAdmission,
) -> Loaded {
    if bytes.len() > max_bytes {
        return Loaded::Unavailable;
    }
    let encoded = match reserve(&budget.encoded, bytes.len() as u32, false) {
        Ok(permit) => permit,
        Err(deferred) => return deferred,
    };
    // Constructing a decoder can inflate metadata before exposing dimensions.
    // Admit that work too, and keep one decoder through conversion. Holding the
    // existing whole budget serializes decoding without partial-reservation
    // upgrades while metadata remains live. Subscription work defers instead
    // of occupying a worker; one-shot user actions wait on their worker.
    let _decode = match admission {
        DecodeAdmission::Defer => match reserve(&budget.decode, DECODE_BUDGET as u32, false) {
            Ok(permit) => permit,
            Err(deferred) => return deferred,
        },
        // One-shot user actions have no subscription to retry Deferred. Wait
        // only for transient decoder ownership on their existing blocking
        // worker, retaining these exact input bytes. Never wait for retained
        // pixels: closing a gallery must not be needed to finish a decode.
        DecodeAdmission::Wait => match iced::futures::executor::block_on(
            budget
                .decode
                .clone()
                .acquire_many_owned(DECODE_BUDGET as u32),
        ) {
            Ok(permit) => permit,
            Err(_) => return Loaded::Unavailable,
        },
    };
    use image::ImageDecoder;
    let Some(input) = bounded_decoder_input(bytes) else {
        return Loaded::Unavailable;
    };
    let Some(mut decoder) = decoder_with_limits(&input, MAX_DECODE_BYTES) else {
        return Loaded::Unavailable;
    };
    let (width, height) = decoder.dimensions();
    let pixel_count = u64::from(width) * u64::from(height);
    let decoded_bytes = decoder.total_bytes();
    if width == 0 || height == 0 || pixel_count > MAX_PIXELS || decoded_bytes > MAX_DECODE_BYTES {
        return Loaded::Unavailable;
    }
    // Codec limits remain best-effort, not a universal RSS/GPU bound. Account
    // conservatively for constructor/metadata scratch alongside decoded pixels
    // and simultaneous RGBA conversion/rotation buffers. An impossible working
    // set is unavailable, never a waiter requesting more than the budget.
    let rgba_bytes = pixel_count * 4;
    let Some(working_bytes) = MAX_DECODE_BYTES
        .checked_add(decoded_bytes)
        .and_then(|total| total.checked_add(rgba_bytes * 2))
    else {
        return Loaded::Unavailable;
    };
    if working_bytes > DECODE_BUDGET as u64 {
        return Loaded::Unavailable;
    }
    // The thumbnail never upscales and fits inside edge², even after orientation.
    let retained_pixels = edge.map_or(pixel_count, |edge| pixel_count.min(u64::from(edge).pow(2)));
    let mut pixels = match reserve(&budget.pixels, (retained_pixels * 4) as u32, true) {
        Ok(permit) => permit,
        Err(deferred) => return deferred,
    };
    let orientation = input.orientation_override.unwrap_or_else(|| {
        decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms)
    });
    let Ok(decoded) = image::DynamicImage::from_decoder(decoder) else {
        return Loaded::Unavailable;
    };
    // Convert before rotating so orientation scratch always uses RGBA8, even
    // for high-depth input. Both operations remain in the bounded worker.
    let mut decoded = image::DynamicImage::ImageRgba8(decoded.into_rgba8());
    decoded.apply_orientation(orientation);
    let rgba = if let Some(edge) = edge
        && (decoded.width() > edge || decoded.height() > edge)
    {
        decoded.thumbnail(edge, edge).into_rgba8()
    } else {
        decoded.into_rgba8()
    };
    let (width, height) = rgba.dimensions();
    let Some(excess) = pixels.num_permits().checked_sub(rgba.len()) else {
        return Loaded::Unavailable;
    };
    // Renderer clones retain exactly the actual thumbnail allocation, rather
    // than its conservative square admission reservation.
    drop(pixels.split(excess));
    drop(bytes);
    drop(encoded);
    let bytes = iced::advanced::graphics::core::Bytes::from_owner(Pixels {
        bytes: rgba.into_raw().into_boxed_slice(),
        _pixels: pixels,
    });
    Loaded::Ready(iced::widget::image::Handle::from_rgba(width, height, bytes))
}
fn read_file(path: impl AsRef<Path>) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // A provider can supply arbitrary paths, including FIFOs. Nonblocking open
    // followed by descriptor metadata rejects special files without waiting.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > chat::MAX_INLINE_IMAGE_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(chat::MAX_INLINE_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= chat::MAX_INLINE_IMAGE_BYTES).then_some(bytes)
}

pub(super) async fn open_external(url: Url) -> Result<(), String> {
    open_external_with(url, open_system_url).await
}

async fn open_external_with<F, Fut>(url: Url, opener: F) -> Result<(), String>
where
    F: FnOnce(Url) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| "The image link does not name a local file".to_owned())?;
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::OpenOptionsExt;
            let checked = || -> std::io::Result<()> {
                let file = std::fs::OpenOptions::new().read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Err(std::io::Error::other("the image is not a regular file"));
                }
                if metadata.len() > chat::MAX_INLINE_IMAGE_BYTES as u64 {
                    return Err(std::io::Error::other("the image exceeds the preview file limit"));
                }
                Ok(())
            };
            checked().map_err(|error| format!("Cannot open image {} on this device: {error}. Paths are literal; choose a concrete existing image.", path.display()))
        }).await.map_err(|error| format!("Could not check the image file: {error}"))??;
    }
    opener(url).await
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct Origin {
    root: PathBuf,
    child: bool,
    device: Option<String>,
}
impl Origin {
    fn of(runtime: &Runtime) -> Self {
        Self {
            root: runtime.session.launch.recipe.working_directory.clone(),
            child: runtime.selected_subagent.is_some(),
            device: crate::device::selected().map(|device| device.name.clone()),
        }
    }
}
fn load(item: &chat::Item, origin: &Origin) -> Loaded {
    load_with_request(item, origin, crate::device_service::request)
}
fn load_with_request(
    item: &chat::Item,
    origin: &Origin,
    request: impl FnOnce(
        crate::device_service::Operation,
    ) -> Result<crate::device_service::Reply, String>,
) -> Loaded {
    let ItemContent::Image {
        path, reference, ..
    } = &item.content
    else {
        return Loaded::Unavailable;
    };
    if let Some(reference) = reference {
        return managed_with_request(reference, origin.device.is_some(), None, request);
    }
    if path.starts_with("data:image/") {
        let bytes = (path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES)
            .then(|| path.split_once(";base64,"))
            .flatten()
            .and_then(|(_, encoded)| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
            });
        return bytes.map_or(Loaded::Unavailable, |bytes| bounded_handle(bytes, budget()));
    }
    load_file_with_request(Path::new(path), origin, DecodeAdmission::Defer, request)
}
fn load_file_with_request(
    path: &Path,
    origin: &Origin,
    admission: DecodeAdmission,
    request: impl FnOnce(
        crate::device_service::Operation,
    ) -> Result<crate::device_service::Reply, String>,
) -> Loaded {
    if path.as_os_str().is_empty() {
        return Loaded::Failed("The attachment does not provide an image file path.".into());
    }
    if path.as_os_str().as_encoded_bytes().len() > 4096 {
        return Loaded::Failed("The attachment's image path exceeds 4096 bytes.".into());
    }
    // Never interpret a child's relative name through its parent's cwd.
    if origin.child && !path.is_absolute() {
        return Loaded::Failed(format!(
            "Cannot preview {path:?}: the child conversation's working directory is unavailable. Ask for a concrete absolute image path."
        ));
    }
    if !path.is_absolute()
        && path
            .to_str()
            .is_some_and(|path| Url::parse(path).is_ok_and(|url| !url.scheme().is_empty()))
    {
        return Loaded::Failed(
            "This attachment needs a concrete image file path or inline image data.".into(),
        );
    }
    let bytes = if origin.device.is_some() {
        // No fallback to the displaying computer, including for an old daemon
        // that does not understand ReadImage. Its namespace is unrelated.
        request(crate::device_service::Operation::ReadImage {
            root: origin.root.clone(),
            path: path.to_owned(),
        })
        .and_then(|reply| match reply {
            crate::device_service::Reply::Image(bytes)
                if bytes.len() <= crate::device_service::MAX_DEVICE_IMAGE_BYTES => Ok(bytes),
            _ => Err("the selected device did not return a supported image preview; use matching cagents executables".into()),
        })
    } else {
        crate::device_service::read_image_file(&origin.root, path, chat::MAX_INLINE_IMAGE_BYTES)
    };
    match bytes {
        Ok(bytes) => match decode_handle_with_admission(
            &bytes,
            budget(),
            None,
            chat::MAX_INLINE_IMAGE_BYTES,
            admission,
        ) {
            Loaded::Unavailable => Loaded::Failed(format!(
                "Cannot preview {path:?} on {}: unsupported, damaged or oversized image.",
                origin.device.as_deref().unwrap_or("this device")
            )),
            loaded => loaded,
        },
        Err(error) => {
            let hint = if path
                .to_string_lossy()
                .chars()
                .any(|ch| matches!(ch, '*' | '?' | '[' | '{' | '}'))
            {
                " Paths are literal; attach each concrete image instead of brace or wildcard shorthand."
            } else {
                ""
            };
            Loaded::Failed(format!(
                "Cannot preview {path:?} on {}: {error}{hint}",
                origin.device.as_deref().unwrap_or("this device")
            ))
        }
    }
}

/// Explicit device-file links share transcript image admission, without
/// listing the containing directory or trying the displaying computer.
pub(in crate::app) async fn file_preview(
    path: PathBuf,
    device: Option<String>,
) -> Result<iced::widget::image::Handle, String> {
    let origin = Origin {
        root: PathBuf::from("/"),
        child: false,
        device,
    };
    let description = format!(
        "{} on {}",
        path.display(),
        origin.device.as_deref().unwrap_or("this device")
    );
    if !path.is_absolute() {
        return Err(format!(
            "Cannot preview {description}: an absolute device path is required."
        ));
    }
    match worker(move || {
        load_file_with_request(
            &path,
            &origin,
            DecodeAdmission::Wait,
            crate::device_service::request,
        )
    })
    .await
    {
        Loaded::Ready(handle) => Ok(handle),
        Loaded::Failed(error) => Err(error),
        Loaded::Unavailable => Err(format!(
            "Cannot preview {description}: unsupported, damaged or oversized image."
        )),
        Loaded::Deferred(_) => Err(format!(
            "Cannot preview {description}: image preview capacity is full. Close other image previews and try again."
        )),
    }
}
/// Managed image identity is resolved by its owning backend. UI path hints
/// never authorize a read, including after history restoration across roots.
fn managed_with_request(
    reference: &chat::ImageReference,
    remote: bool,
    edge: Option<u32>,
    request: impl FnOnce(
        crate::device_service::Operation,
    ) -> Result<crate::device_service::Reply, String>,
) -> Loaded {
    if let Err(error) = reference.validate() {
        return Loaded::Failed(error);
    }
    let bytes = if remote {
        request(crate::device_service::Operation::ReadImageAsset { reference: reference.clone() })
            .and_then(|reply| match reply {
                crate::device_service::Reply::Image(bytes) if bytes.len() <= crate::device_service::MAX_DEVICE_IMAGE_BYTES => Ok(bytes),
                _ => Err("The selected device did not return a supported image preview; use matching cagents executables.".into()),
            })
    } else {
        chat::attachment_assets::read(reference, crate::device_service::MAX_DEVICE_IMAGE_BYTES)
    };
    match bytes {
        Ok(bytes) => match bounded_handle_size(bytes, budget(), edge) {
            Loaded::Unavailable => Loaded::Failed(format!(
                "Cannot preview {}: unsupported, damaged, or larger than the 16 MiB preview limit.",
                reference.label()
            )),
            loaded => loaded,
        },
        Err(error) => Loaded::Failed(format!(
            "Cannot preview {} on its originating device: {error}",
            reference.label()
        )),
    }
}
pub(super) fn managed_thumbnail(reference: &chat::ImageReference) -> Loaded {
    managed_with_request(
        reference,
        crate::device::is_remote(),
        Some(128),
        crate::device_service::request,
    )
}

#[cfg(test)]
fn bytes_handle(bytes: Vec<u8>) -> Option<iced::widget::image::Handle> {
    match bounded_handle(bytes, budget()) {
        Loaded::Ready(handle) => Some(handle),
        _ => None,
    }
}

#[derive(Clone)]
pub(super) struct Source {
    target: Target,
    origin: Origin,
    item: Arc<chat::Item>,
    attempt: u64,
    wait: Option<Pressure>,
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.origin == other.origin
            && Arc::ptr_eq(&self.item, &other.item)
            && self.attempt == other.attempt
            && self.wait == other.wait
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
        self.target.hash(state);
        self.origin.hash(state);
        Arc::as_ptr(&self.item).hash(state);
        self.attempt.hash(state);
        self.wait.hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let source = source.clone();
    Box::pin(iced::futures::stream::once(async move {
        if let Some(pressure) = source.wait {
            pressure.available().await;
        }
        let item = source.item.clone();
        let loaded = worker(move || load(&item, &source.origin)).await;
        action(Action::ImageLoaded(
            source.target,
            source.item,
            source.attempt,
            loaded,
        ))
    }))
}
/// Transcript previews and composer thumbnails share worker admission. A
/// cancelled subscription cannot release a slot while its blocking work runs.
pub(super) async fn worker(load: impl FnOnce() -> Loaded + Send + 'static) -> Loaded {
    run_worker(load).await.unwrap_or(Loaded::Unavailable)
}

/// Staging pasted/selected images shares both worker and memory admission
/// with previews; cancelling the UI task cannot release running blocking work.
pub(super) async fn run_worker<T: Send + 'static>(
    load: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    static WORKERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let workers = WORKERS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)));
    let permit = workers
        .clone()
        .acquire_owned()
        .await
        .expect("image workers remain open");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        load()
    })
    .await
    .map_err(|error| format!("Image processing failed: {error}"))
}

/// Fully decode before publishing durable bytes, with the existing global
/// admission limits. Only a 1-pixel temporary preview is retained and dropped.
pub(super) fn validate_attachment(
    bytes: &[u8],
    max_bytes: u64,
) -> Result<image::ImageFormat, String> {
    if bytes.is_empty()
        || bytes.len() as u64 > max_bytes.min(chat::attachment_assets::MAX_IMAGE_BYTES)
    {
        return Err("Image exceeds this provider's image size limit.".into());
    }
    let format = image::guess_format(bytes).map_err(|_| "The file is not a supported image.")?;
    if !matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP
    ) {
        return Err("The file is not a supported attachment image.".into());
    }
    match decode_handle_with_admission(
        bytes,
        budget(),
        Some(1),
        chat::attachment_assets::MAX_IMAGE_BYTES as usize,
        DecodeAdmission::Wait,
    ) {
        Loaded::Ready(_) => Ok(format),
        Loaded::Deferred(_) => Err("Image memory is busy. Try attaching the image again.".into()),
        Loaded::Unavailable | Loaded::Failed(_) => {
            Err("The image is damaged or exceeds the image dimensions limit.".into())
        }
    }
}

pub(super) fn thumbnail(path: &Path) -> Loaded {
    read_file(path).map_or(Loaded::Unavailable, |bytes| {
        bounded_handle_size(bytes, budget(), Some(128))
    })
}

fn items(runtime: &Runtime) -> &[Arc<chat::Item>] {
    runtime
        .selected_subagent
        .as_deref()
        .and_then(|id| {
            runtime
                .snapshot
                .subagents
                .iter()
                .find(|agent| agent.id == id)
                .map(|agent| agent.items.as_slice())
        })
        .unwrap_or(&runtime.snapshot.items)
}
fn recent(runtime: &Runtime) -> Vec<&Arc<chat::Item>> {
    items(runtime)
        .iter()
        .rev()
        .filter(|item| matches!(item.content, ItemContent::Image { .. }))
        .take(RECENT_IMAGES)
        .collect()
}
/// One explicit request outranks the newest automatic candidates. Keeping
/// this interest bounded prevents browsing old images from pinning the whole
/// conversation, and one worker per transcript admits recent images in order.
fn interest(runtime: &Runtime) -> Vec<&Arc<chat::Item>> {
    if !runtime.visible.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let requested = runtime
        .preview_request
        .as_deref()
        .and_then(|id| items(runtime).iter().find(|item| item.id == id));
    requested
        .into_iter()
        .chain(
            recent(runtime)
                .into_iter()
                .filter(|item| requested.is_none_or(|requested| requested.id != item.id)),
        )
        .collect()
}
fn retire(cached: &mut CachedItem, evicted: bool) {
    cached.image = None;
    cached.image_source = None;
    cached.image_wait = None;
    cached.image_evicted = evicted;
    cached.image_attempt = cached.image_attempt.wrapping_add(1);
}
pub(super) fn sync(runtime: &mut Runtime) {
    if !runtime.visible.load(Ordering::Relaxed)
        || runtime
            .preview_request
            .as_deref()
            .is_some_and(|id| !items(runtime).iter().any(|item| item.id == id))
    {
        runtime.preview_request = None;
    }
    let interested: HashSet<_> = interest(runtime)
        .into_iter()
        .map(|item| item.id.clone())
        .collect();
    let parent = runtime.selected_subagent.is_none();
    for (active, id, cached) in runtime
        .cache
        .iter_mut()
        .map(|(id, cached)| (parent, id, cached))
        .chain(
            runtime
                .child_cache
                .iter_mut()
                .map(|(id, cached)| (!parent, id, cached)),
        )
    {
        if cached.image_item.is_none() && cached.image_source.is_none() && cached.image.is_none() {
            continue;
        }
        if !active || !interested.contains(id) {
            retire(cached, false);
        } else if cached.image.is_none()
            && cached.image_source.is_none()
            && !cached.image_unavailable
            && !cached.image_evicted
        {
            cached.image_source = cached.image_item.clone();
            cached.image_attempt = cached.image_attempt.wrapping_add(1);
        }
    }
}
pub(super) fn request(runtime: &mut Runtime, id: &str) {
    if !runtime.visible.load(Ordering::Relaxed) {
        return;
    }
    let Some(item) = items(runtime)
        .iter()
        .find(|item| item.id == id && matches!(item.content, ItemContent::Image { .. }))
        .cloned()
    else {
        return;
    };
    if !runtime.presentation_cache().contains_key(id) {
        return;
    }
    runtime.preview_request = Some(id.to_owned());
    if let Some(cached) = runtime.presentation_cache_mut().get_mut(id) {
        cached.image_item = Some(item.clone());
        if cached.image.is_none() {
            cached.image_source = Some(item);
        }
        cached.image_attempt = cached.image_attempt.wrapping_add(1);
        cached.image_wait = None;
        cached.image_evicted = false;
        cached.image_unavailable = false;
        cached.image_error = None;
    }
    sync(runtime);
}
fn pixel_bytes(cached: &CachedItem) -> usize {
    match cached.image.as_ref() {
        Some(iced::widget::image::Handle::Rgba { pixels, .. }) => pixels.as_ref().len(),
        _ => 0,
    }
}
/// Retire the oldest application owners only until the requested admission
/// can fit. Retained trees may still hold those bytes; the waiter observes the
/// actual semaphore rather than treating this estimate as released capacity.
pub(super) fn evict_ready(runtime: &mut Runtime, mut missing: usize) -> usize {
    let order: Vec<_> = interest(runtime)
        .into_iter()
        .map(|item| item.id.clone())
        .collect();
    for id in order.iter().rev() {
        if missing == 0 {
            break;
        }
        if let Some(cached) = runtime.presentation_cache_mut().get_mut(id)
            && cached.image.is_some()
        {
            missing = missing.saturating_sub(pixel_bytes(cached));
            retire(cached, true);
        }
    }
    missing
}
pub(super) fn complete(
    runtime: &mut Runtime,
    item: &Arc<chat::Item>,
    attempt: u64,
    loaded: Loaded,
) -> Option<usize> {
    let interested = interest(runtime);
    if !interested
        .iter()
        .any(|current| current.id == item.id && current.content == item.content)
    {
        return None;
    }
    let order: Vec<_> = interested.into_iter().map(|item| item.id.clone()).collect();
    let Some(priority) = order.iter().position(|id| *id == item.id) else {
        return None;
    };
    let Some(cached) = runtime.presentation_cache().get(&item.id) else {
        return None;
    };
    if cached.image_attempt != attempt
        || cached
            .image_source
            .as_ref()
            .is_none_or(|source| !Arc::ptr_eq(source, item))
        || cached
            .image_item
            .as_ref()
            .is_some_and(|current| current.content != item.content)
    {
        return None;
    }
    let explicit = runtime.preview_request.as_deref() == Some(item.id.as_str());
    if let Loaded::Deferred(pressure) = loaded {
        let mut missing =
            (pressure.permits as usize).saturating_sub(pressure.resource.available_permits());
        let mut evicted = false;
        if pressure.retained {
            for id in order[priority + 1..].iter().rev() {
                if missing == 0 {
                    break;
                }
                if let Some(cached) = runtime.presentation_cache_mut().get_mut(id)
                    && cached.image.is_some()
                {
                    missing = missing.saturating_sub(pixel_bytes(cached));
                    retire(cached, true);
                    evicted = true;
                }
            }
        }
        let cached = runtime.presentation_cache_mut().get_mut(&item.id).unwrap();
        if !pressure.retained || explicit || evicted || cached.image_wait.is_some() {
            cached.image_wait = Some(pressure);
            cached.image_attempt = cached.image_attempt.wrapping_add(1);
        } else {
            // Lower-priority automatic candidates cannot displace the images
            // already admitted. They stay available through Load preview.
            retire(cached, true);
        }
        return (explicit
            && missing > 0
            && cached.image_wait.as_ref().is_some_and(|wait| wait.retained))
        .then_some(missing);
    }
    let cached = runtime.presentation_cache_mut().get_mut(&item.id).unwrap();
    cached.image_source = None;
    cached.image_wait = None;
    cached.image_unavailable = matches!(loaded, Loaded::Unavailable | Loaded::Failed(_));
    cached.image_error = match &loaded {
        Loaded::Failed(error) => Some(error.clone()),
        _ => None,
    };
    cached.image = match loaded {
        Loaded::Ready(handle) => Some(handle),
        _ => None,
    };
    None
}
pub(super) fn subscriptions(runtime: &Runtime) -> Vec<Subscription<Message>> {
    interest(runtime)
        .into_iter()
        .find_map(|item| {
            let cached = runtime.presentation_cache().get(&item.id)?;
            let source = cached.image_source.as_ref()?;
            Some(Subscription::run_with(
                Source {
                    target: runtime.target,
                    origin: Origin::of(runtime),
                    item: source.clone(),
                    attempt: cached.image_attempt,
                    wait: cached.image_wait.clone(),
                },
                stream,
            ))
        })
        .into_iter()
        .collect()
}

struct Entry {
    id: String,
    managed: bool,
    path: String,
    label: String,
    handle: iced::widget::image::Handle,
}
pub(in crate::app) struct Gallery {
    entries: Vec<Entry>,
    selected: usize,
    remote: bool,
    pub(super) reset: u64,
}
impl Gallery {
    pub(in crate::app) fn file(
        path: PathBuf,
        device: Option<&str>,
        handle: iced::widget::image::Handle,
    ) -> Self {
        Self {
            entries: vec![Entry {
                id: "file".into(),
                managed: false,
                // Keep non-UTF8 filenames and literal URL characters exact.
                path: Url::from_file_path(&path)
                    .map(|url| url.to_string())
                    .unwrap_or_default(),
                label: format!("{} · {}", device.unwrap_or("This device"), path.display()),
                handle,
            }],
            selected: 0,
            remote: device.is_some(),
            reset: 0,
        }
    }
    pub(super) fn new(runtime: &Runtime, id: &str) -> Option<Self> {
        let items = match runtime.selected_subagent.as_deref() {
            Some(agent) => {
                &runtime
                    .snapshot
                    .subagents
                    .iter()
                    .find(|a| a.id == agent)?
                    .items
            }
            None => &runtime.snapshot.items,
        };
        let entries: Vec<_> = items
            .iter()
            .filter_map(|item| {
                let ItemContent::Image {
                    path,
                    description,
                    reference,
                } = &item.content
                else {
                    return None;
                };
                Some(Entry {
                    id: item.id.clone(),
                    managed: reference.is_some(),
                    path: if Path::new(path).is_absolute() || path.starts_with("data:image/") {
                        path.clone()
                    } else {
                        runtime
                            .session
                            .launch
                            .recipe
                            .working_directory
                            .join(path)
                            .to_string_lossy()
                            .into_owned()
                    },
                    label: if description.is_empty() {
                        "Image".into()
                    } else {
                        description.clone()
                    },
                    handle: runtime.presentation_cache().get(&item.id)?.image.clone()?,
                })
            })
            .collect();
        let selected = entries.iter().position(|entry| entry.id == id)?;
        Some(Self {
            entries,
            selected,
            remote: crate::device::is_remote(),
            reset: 0,
        })
    }
    pub(in crate::app) fn next(&mut self, forward: bool) {
        self.selected = if forward {
            (self.selected + 1) % self.entries.len()
        } else {
            (self.selected + self.entries.len() - 1) % self.entries.len()
        };
        self.reset = self.reset.wrapping_add(1);
    }
    pub(super) fn url(&self) -> Option<Url> {
        let entry = &self.entries[self.selected];
        // Managed snapshot paths are non-authority hints. The in-app viewer
        // owns verified pixels; never hand an unverified hint to another app.
        if entry.managed {
            return None;
        }
        let path = &entry.path;
        if Path::new(path).is_absolute() {
            (!self.remote)
                .then(|| Url::from_file_path(path).ok())
                .flatten()
        } else {
            Url::parse(path).ok().filter(|url| {
                matches!(url.scheme(), "http" | "https") || !self.remote && url.scheme() == "file"
            })
        }
    }
    pub(in crate::app) fn view(&self) -> Element<'_, Message> {
        let entry = &self.entries[self.selected];
        let navigation = self.entries.len() > 1;
        let toolbar = widget::row::with_children(vec![
            icon(
                Icon::ChevronLeft,
                "Previous image (Left)",
                navigation,
                Action::ImageNext(false),
            ),
            design::caption(format!("{} / {}", self.selected + 1, self.entries.len())).into(),
            icon(
                Icon::ChevronRight,
                "Next image (Right)",
                navigation,
                Action::ImageNext(true),
            ),
            iced::widget::Space::new().width(Length::Fill).into(),
            crate::ui_controls::button::secondary("Fit")
                .on_press(action(Action::ImageFit))
                .into(),
            crate::ui_controls::button::secondary("Open externally")
                .on_press_maybe(self.url().map(|_| action(Action::ImageExternal)))
                .into(),
            icon(Icon::Close, "Close image (Esc)", true, Action::CloseImage),
        ])
        .spacing(step::GAP_S)
        .align_y(iced::Alignment::Center);
        let viewer: Element<'_, Message> = iced::widget::image::Viewer::new(entry.handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(iced::ContentFit::Contain)
            .min_scale(0.1)
            .max_scale(20.0)
            .into();
        let canvas = canvas::resettable(self.reset, viewer);
        dialog_ui::overlay(
            dialog_ui::surface(
                widget::column::with_children(vec![
                    toolbar.into(),
                    design::caption(entry.label.as_str()).into(),
                    canvas.into(),
                    design::caption("Scroll to zoom · Drag to pan · Left / Right to browse").into(),
                ])
                .spacing(step::GAP_S)
                .height(Length::Fill),
                f32::MAX,
            ),
            Some(action(Action::CloseImage)),
            1.0,
        )
    }
}
impl Runtime {
    pub(super) fn image_preview(
        &self,
        item: &chat::Item,
        cached: &CachedItem,
    ) -> Element<'static, Message> {
        let label = match &item.content {
            ItemContent::Image { description, .. } if !description.is_empty() => {
                description.clone()
            }
            _ => "Image".into(),
        };
        let Some(handle) = &cached.image else {
            return if cached.image_unavailable {
                design::caption(
                    cached
                        .image_error
                        .clone()
                        .unwrap_or_else(|| format!("{label} · Preview unavailable")),
                )
                .into()
            } else if cached.image_wait.is_some() {
                design::caption(format!("{label} · Waiting for preview memory…")).into()
            } else if cached.image_source.is_some()
                && interest(self)
                    .iter()
                    .any(|candidate| candidate.id == item.id)
            {
                design::caption(format!("{label} · Loading preview…")).into()
            } else {
                widget::column::with_children(vec![
                    design::caption(label).into(),
                    crate::ui_controls::button::secondary("Load preview")
                        .on_press(action(Action::LoadImage(self.target, item.id.clone())))
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .into()
            };
        };
        let preview = iced::widget::image(handle.clone())
            .width(Length::Fill)
            .height(280)
            .content_fit(iced::ContentFit::Contain);
        widget::column::with_children(vec![
            widget::button::custom(preview)
                .class(crate::ui_controls::control_style(false, None))
                .padding(0)
                .on_press(action(Action::OpenImage(self.target, item.id.clone())))
                .width(Length::Fill)
                .into(),
            design::caption(format!("{label} · Click to enlarge")).into(),
        ])
        .spacing(step::GAP_XS)
        .width(Length::Fill)
        .into()
    }
}

#[cfg(test)]
pub(super) fn deferred_for_test() -> Loaded {
    Loaded::Deferred(Pressure {
        resource: Arc::new(tokio::sync::Semaphore::new(0)),
        permits: 1,
        retained: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_headers_bound_pixel_memory_and_reject_invalid_data() {
        let mut bytes = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut bytes),
            &[0, 0, 0, 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        assert!(bytes_handle(bytes).is_some());
        assert!(bytes_handle(b"not an image".to_vec()).is_none());
        // GIF dimensions live in the small logical-screen header. This needs
        // no huge pixel buffer to check a highly compressed image's bound.
        let huge = b"GIF89a\xff\xff\xff\xff\x00\x00\x00;";
        assert!(bytes_handle(huge.to_vec()).is_none());
    }
    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut bytes),
            &[1, 2, 3, 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        bytes
    }
    fn image_item(path: String) -> chat::Item {
        chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Image {
                path,
                description: "Fixture".into(),
                reference: None,
            },
        }
    }
    #[test]
    fn managed_remote_preview_uses_backend_receipt_across_different_roots_and_never_local_hint() {
        chat::attachment_assets::with_test_image(|backend_image| {
            let ui_root = tempfile::tempdir().unwrap();
            let ui_hint = ui_root.path().join("coincident-image.png");
            let ui_pixels = png();
            std::fs::write(&ui_hint, &ui_pixels).unwrap();
            assert_ne!(backend_image.path.parent(), Some(ui_root.path()));
            let backend_bytes = std::fs::read(&backend_image.path).unwrap();
            let expected = image::load_from_memory(&backend_bytes)
                .unwrap()
                .into_rgba8()
                .into_raw();
            assert_ne!(
                expected,
                image::load_from_memory(&ui_pixels)
                    .unwrap()
                    .into_rgba8()
                    .into_raw()
            );
            let reference = backend_image.reference.as_ref().unwrap();
            let mut item = image_item(ui_hint.to_string_lossy().into_owned());
            if let ItemContent::Image {
                reference: slot, ..
            } = &mut item.content
            {
                *slot = Some(reference.clone());
            }
            let origin = Origin {
                root: ui_root.path().into(),
                child: false,
                device: Some("backend-with-another-home".into()),
            };
            let loaded = load_with_request(&item, &origin, |operation| {
                let crate::device_service::Operation::ReadImageAsset { reference: sent } =
                    operation
                else {
                    panic!("managed preview must not send any caller path");
                };
                assert_eq!(&sent, reference);
                chat::attachment_assets::read(&sent, crate::device_service::MAX_DEVICE_IMAGE_BYTES)
                    .map(crate::device_service::Reply::Image)
            });
            let Loaded::Ready(iced::widget::image::Handle::Rgba { pixels, .. }) = loaded else {
                panic!("valid backend receipt bytes must decode");
            };
            assert_eq!(pixels.as_ref(), expected.as_slice());
            let mut foreign = reference.clone();
            foreign.device = uuid::Uuid::new_v4();
            if let ItemContent::Image {
                reference: slot, ..
            } = &mut item.content
            {
                *slot = Some(foreign);
            }
            let loaded = load_with_request(&item, &origin, |operation| {
                let crate::device_service::Operation::ReadImageAsset { reference } = operation
                else {
                    panic!("no path fallback");
                };
                chat::attachment_assets::read(
                    &reference,
                    crate::device_service::MAX_DEVICE_IMAGE_BYTES,
                )
                .map(crate::device_service::Reply::Image)
            });
            assert!(matches!(loaded, Loaded::Failed(error) if error.contains("device")));
            assert_eq!(
                std::fs::read(ui_hint).unwrap(),
                ui_pixels,
                "foreign receipt never opens or mutates coincident UI pixels"
            );
        });
    }

    #[test]
    fn remote_preview_never_reads_same_named_local_pixels_or_falls_back_after_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("same-path.png");
        std::fs::write(&path, png()).unwrap();
        let origin = Origin {
            root: directory.path().into(),
            child: false,
            device: Some("fixture-device".into()),
        };
        let item = image_item(path.to_string_lossy().into_owned());
        let failed = load_with_request(&item, &origin, |operation| {
            assert!(
                matches!(operation, crate::device_service::Operation::ReadImage { root, path: sent }
                if root == directory.path() && sent == path)
            );
            Err("file missing on device".into())
        });
        assert!(
            matches!(failed, Loaded::Failed(error) if error.contains("same-path.png")
            && error.contains("fixture-device") && error.contains("file missing on device"))
        );
        // A reachable older daemon returning an unsupported reply is equally
        // unable to authorize pixels from this computer's identically named file.
        assert!(
            matches!(load_with_request(&item, &origin, |_| Ok(crate::device_service::Reply::Saved)),
            Loaded::Failed(error) if error.contains("matching cagents executables"))
        );
        let invalid_remote = load_with_request(&item, &origin, |_| {
            Ok(crate::device_service::Reply::Image(
                b"not remote pixels".to_vec(),
            ))
        });
        assert!(matches!(invalid_remote, Loaded::Failed(_)));
        assert!(matches!(
            load_with_request(&item, &origin, |_| Ok(crate::device_service::Reply::Image(
                png()
            ))),
            Loaded::Ready(_)
        ));
    }
    #[test]
    fn explicit_device_image_links_keep_exact_paths_pixels_and_gallery_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("界 #% literal-*.PNG");
        std::fs::write(&path, png()).unwrap();
        let mut remote = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut remote),
            &[3, 240, 3, 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        let origin = Origin {
            root: "/".into(),
            child: false,
            device: Some("FW16 fixture".into()),
        };
        let Loaded::Ready(handle) = load_file_with_request(
            &path,
            &origin,
            DecodeAdmission::Defer,
            |operation| {
                assert!(
                    matches!(operation, crate::device_service::Operation::ReadImage { root, path: sent }
                if root == Path::new("/") && sent == path)
                );
                Ok(crate::device_service::Reply::Image(remote))
            },
        ) else {
            panic!("real remote PNG should decode")
        };
        let iced::widget::image::Handle::Rgba { pixels, .. } = &handle else {
            panic!("decoded pixels")
        };
        assert_eq!(
            pixels.as_ref(),
            &[3, 240, 3, 255],
            "remote pixels must win over the actual different local file"
        );
        let remote = Gallery::file(path.clone(), Some("FW16 fixture"), handle.clone());
        assert!(
            remote.url().is_none(),
            "a remote file cannot launch a local viewer"
        );
        assert!(remote.entries[0].label.contains("FW16 fixture"));
        let local = Gallery::file(path.clone(), None, handle.clone());
        assert!(!remote.entries[0].managed && !local.entries[0].managed);
        assert_eq!(local.url().unwrap().to_file_path().unwrap(), path);

        use std::os::unix::ffi::OsStringExt;
        let opaque = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"image-\xff.png".to_vec()));
        let gallery = Gallery::file(opaque.clone(), None, handle);
        assert_eq!(
            gallery.url().unwrap().to_file_path().unwrap(),
            opaque,
            "file URLs retain non-UTF8 names without a lossy round trip"
        );
    }
    #[test]
    fn concrete_local_names_use_conversation_cwd_and_unknown_child_cwd_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("cwd");
        std::fs::create_dir_all(cwd.join("docs")).unwrap();
        std::fs::write(cwd.join("docs/image.png"), png()).unwrap();
        std::fs::write(directory.path().join("image.png"), png()).unwrap();
        let origin = Origin {
            root: cwd,
            child: false,
            device: None,
        };
        let item = image_item("docs/image.png".into());
        assert!(matches!(
            load_with_request(&item, &origin, |_| panic!(
                "local preview must not request device transport"
            )),
            Loaded::Ready(_)
        ));
        let child = Origin {
            child: true,
            ..origin.clone()
        };
        assert!(
            matches!(load_with_request(&item, &child, |_| panic!("unknown child cwd must not be requested")),
            Loaded::Failed(error) if error.contains("child conversation's working directory"))
        );
        assert!(matches!(
            load(&image_item("../image.png".into()), &origin),
            Loaded::Ready(_)
        ));
        for path in ["docs/image-*.png", "docs/image-{a,b}.png"] {
            assert!(matches!(
                load(&image_item(path.into()), &origin),
                Loaded::Failed(_)
            ));
        }
    }
    #[test]
    fn inline_images_do_not_consult_any_device_filesystem() {
        let origin = Origin {
            root: "/unavailable".into(),
            child: true,
            device: Some("remote".into()),
        };
        let path = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png())
        );
        assert!(matches!(
            load_with_request(&image_item(path), &origin, |_| panic!(
                "inline images have no file owner"
            )),
            Loaded::Ready(_)
        ));
    }

    #[tokio::test]
    async fn external_image_open_checks_the_exact_file_before_launching_a_viewer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tree-real-*.png");
        let url = Url::from_file_path(&path).unwrap();
        let error = open_external_with(url.clone(), |_| async {
            panic!("a missing pattern must never reach a viewer")
        })
        .await
        .unwrap_err();
        assert!(error.contains("tree-real-*.png") && error.contains("Paths are literal"));
        std::fs::write(&path, png()).unwrap();
        open_external_with(url, |found| async move {
            assert_eq!(
                found.to_file_path().unwrap(),
                path,
                "literal metacharacters are preserved"
            );
            Ok(())
        })
        .await
        .unwrap();
        let folder = Url::from_file_path(directory.path()).unwrap();
        assert!(
            open_external_with(folder, |_| async { panic!("a directory is not an image") })
                .await
                .is_err()
        );
    }

    #[test]
    fn managed_gallery_cannot_open_a_different_local_hint_externally() {
        chat::attachment_assets::with_test_image(|backend_image| {
            let display = tempfile::tempdir().unwrap();
            let hint = display.path().join("coincident.png");
            std::fs::write(&hint, png()).unwrap();
            let backend_bytes = std::fs::read(&backend_image.path).unwrap();
            assert_ne!(backend_bytes, png());
            let mut app = crate::app::tests::context_inspector_test_app(false);
            drop(app.new_native_chat(Provider::Codex, Some(display.path().into())));
            let card = app.workspace.focused().unwrap();
            let CardRuntime::Chat(runtime) = app.cards.get_mut(&card).unwrap() else {
                panic!("draft chat");
            };
            let mut item = image_item(hint.to_string_lossy().into_owned());
            if let ItemContent::Image { reference, .. } = &mut item.content {
                *reference = backend_image.reference.clone();
            }
            let item = Arc::new(item);
            let mut snapshot = (*runtime.snapshot).clone();
            snapshot.items = vec![item.clone()];
            runtime.snapshot = Arc::new(snapshot);
            let mut cached =
                CachedItem::new(&item.content, crate::settings::EditorSyntaxTheme::Auto);
            cached.image = bytes_handle(backend_bytes);
            runtime.cache.insert(item.id.clone(), cached);
            let gallery = Gallery::new(runtime, &item.id).unwrap();
            assert!(gallery.entries[0].managed);
            assert!(
                gallery.url().is_none(),
                "verified internal pixels must never authorize a coincident external hint"
            );
        });
    }

    #[test]
    fn inline_image_gallery_never_offers_a_filesystem_external_link() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(Provider::Codex, Some(PathBuf::from("/tmp/gallery"))));
        let card = app.workspace.focused().unwrap();
        let CardRuntime::Chat(runtime) = app.cards.get_mut(&card).unwrap() else {
            panic!("draft chat")
        };
        let path = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png())
        );
        let item = Arc::new(image_item(path.clone()));
        let mut snapshot = (*runtime.snapshot).clone();
        snapshot.items = vec![item.clone()];
        runtime.snapshot = Arc::new(snapshot);
        let mut cached = CachedItem::new(&item.content, crate::settings::EditorSyntaxTheme::Auto);
        cached.image = bytes_handle(png());
        runtime.cache.insert(item.id.clone(), cached);
        let gallery = Gallery::new(runtime, &item.id).unwrap();
        assert_eq!(gallery.entries[0].path, path);
        assert!(
            gallery.url().is_none(),
            "inline pixels are not a local filename"
        );
    }
    #[test]
    fn file_preview_rejects_fifos_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("provider-image");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || {
            sender.send(read_file(path.to_str().unwrap())).unwrap();
        });
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("FIFO preview must not block")
                .is_none()
        );
        task.join().unwrap();
        assert!(read_file(directory.path().to_str().unwrap()).is_none());
    }
    #[test]
    fn file_preview_retains_the_validated_bytes_after_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        let original = png();
        std::fs::write(&path, &original).unwrap();
        let bytes = read_file(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"different bytes after validation").unwrap();
        let Loaded::Ready(iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels: actual,
            ..
        }) = bounded_handle(bytes, &Budget::new(1024, 4))
        else {
            panic!("file previews must retain bytes, never reopen paths")
        };
        assert_eq!((width, height), (1, 1));
        assert_eq!(actual.as_ref(), &[1, 2, 3, 255]);
    }
    #[test]
    fn composer_thumbnail_retains_only_small_exact_rgba_and_immutable_pixels() {
        let mut bytes = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut bytes),
            &vec![127; 512 * 256 * 4],
            512,
            256,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        let budget = Budget::new(bytes.len(), 128 * 128 * 4);
        let Loaded::Ready(handle) = bounded_handle_size(bytes.clone(), &budget, Some(128)) else {
            panic!("thumbnail fits although full pixels do not fit retained budget")
        };
        let iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = &handle
        else {
            panic!("renderer must not open a path or decode bytes")
        };
        assert_eq!((*width, *height), (128, 64));
        assert_eq!(pixels.as_ref().len(), 128 * 64 * 4);
        assert!(pixels.as_ref().iter().all(|byte| *byte == 127));
        assert_eq!(budget.pixels.available_permits(), 128 * 64 * 4);
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        let retained = handle.clone();
        drop(handle);
        assert_eq!(budget.pixels.available_permits(), 128 * 64 * 4);
        drop(retained);
        assert_eq!(budget.pixels.available_permits(), 128 * 128 * 4);
    }
    #[test]
    fn composer_thumbnail_rejects_fifo_and_preserves_non_utf8_file_paths() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = std::thread::spawn(move || sender.send(thumbnail(&fifo)).unwrap());
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("FIFO thumbnail cannot block"),
            Loaded::Unavailable
        ));
        task.join().unwrap();
        let path = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"image-\xff.png".to_vec()));
        std::fs::write(&path, png()).unwrap();
        assert!(matches!(
            thumbnail(&path),
            Loaded::Ready(iced::widget::image::Handle::Rgba {
                width: 1,
                height: 1,
                ..
            })
        ));
    }
    #[test]
    fn image_budget_covers_every_handle_clone_and_releases_after_the_last() {
        let bytes = png();
        let budget = Budget::new(bytes.len(), 4);
        let Loaded::Ready(handle) = bounded_handle(bytes.clone(), &budget) else {
            panic!("first image fits")
        };
        let retained = handle.clone();
        drop(handle);
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Deferred(_)
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.pixels.available_permits(), 0);
        drop(retained);
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.pixels.available_permits(), 4);
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Ready(_)
        ));
        // Failed pixel reservation must return its encoded-byte reservation.
        let budget = Budget::new(bytes.len(), 3);
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Deferred(_)
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.pixels.available_permits(), 3);
    }
    fn ico_bytes(entries: &[(u8, u8, u16, &[u8])]) -> Vec<u8> {
        let mut bytes = b"\0\0\x01\0".to_vec();
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut offset = 6 + entries.len() * 16;
        for &(width, height, depth, image) in entries {
            bytes.extend_from_slice(&[width, height, 0, 0]);
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&depth.to_le_bytes());
            bytes.extend_from_slice(&(image.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += image.len();
        }
        for &(_, _, _, image) in entries {
            bytes.extend_from_slice(image);
        }
        bytes
    }
    fn ico_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }
    fn assert_ico_pixels(bytes: Vec<u8>, dimensions: (u32, u32), expected: &[u8]) {
        let budget = Budget::new(bytes.len(), expected.len());
        let encoded_length = bytes.len();
        let Loaded::Ready(iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        }) = bounded_handle(bytes, &budget)
        else {
            panic!("valid ICO must decode");
        };
        assert_eq!((width, height), dimensions);
        assert_eq!(pixels.as_ref(), expected);
        assert_eq!(budget.encoded.available_permits(), encoded_length);
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 0);
        drop(pixels);
        assert_eq!(budget.pixels.available_permits(), expected.len());
    }
    #[test]
    fn ico_preview_preserves_selection_ties_and_validates_the_whole_directory() {
        let red = ico_png(1, 1, &[255, 0, 0, 255]);
        let green = ico_png(1, 1, &[0, 255, 0, 255]);
        let blue = ico_png(2, 1, &[0, 0, 255, 255, 0, 0, 255, 255]);
        for (entries, selected) in [
            (
                vec![(1, 1, 32, red.as_slice()), (2, 1, 24, blue.as_slice())],
                red.as_slice(),
            ),
            (
                vec![(1, 1, 32, red.as_slice()), (2, 1, 32, blue.as_slice())],
                blue.as_slice(),
            ),
            (
                vec![(1, 1, 32, red.as_slice()), (1, 1, 32, green.as_slice())],
                green.as_slice(),
            ),
            (
                vec![
                    (1, 1, 32, red.as_slice()),
                    (1, 1, 32, green.as_slice()),
                    (2, 1, 24, blue.as_slice()),
                ],
                red.as_slice(),
            ),
        ] {
            let bytes = ico_bytes(&entries);
            assert_eq!(bounded_decoder_input(&bytes).unwrap().bytes, selected);
        }
        assert_ico_pixels(ico_bytes(&[(1, 1, 32, &red)]), (1, 1), &[255, 0, 0, 255]);
        let bytes = ico_bytes(&[(1, 1, 24, &red), (1, 1, 32, &green)]);
        let mut unused_bad_range = bytes.clone();
        unused_bad_range[14..22].fill(255);
        assert_eq!(
            bounded_decoder_input(&unused_bad_range).unwrap().bytes,
            green
        );
        for field in [10, 12] {
            let mut invalid = bytes.clone();
            invalid[field..field + 2].copy_from_slice(&257u16.to_le_bytes());
            assert!(bounded_decoder_input(&invalid).is_none());
            invalid[field..field + 2].copy_from_slice(&256u16.to_le_bytes());
            assert!(bounded_decoder_input(&invalid).is_some());
        }
        let single = ico_bytes(&[(1, 1, 32, &red)]);
        let mut empty = single.clone();
        empty[4..6].fill(0);
        let mut short_directory = single[..21].to_vec();
        short_directory[4..6].copy_from_slice(&2u16.to_le_bytes());
        let mut bad_offset = single.clone();
        bad_offset[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut bad_length = single.clone();
        bad_length[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut short_png = single;
        short_png[14..18].copy_from_slice(&7u32.to_le_bytes());
        for invalid in [
            b"\0\0\x01\0".to_vec(),
            empty,
            short_directory,
            bad_offset,
            bad_length,
            short_png,
        ] {
            assert!(bounded_decoder_input(&invalid).is_none());
            let budget = Budget::new(invalid.len(), 4);
            let encoded_length = invalid.len();
            assert!(matches!(
                bounded_handle(invalid, &budget),
                Loaded::Unavailable
            ));
            assert_eq!(budget.encoded.available_permits(), encoded_length);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget.pixels.available_permits(), 4);
        }
    }
    #[test]
    fn ico_preview_preserves_png_checks_orientation_and_dib_mask() {
        use image::ImageEncoder;
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
        let mut oriented = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut oriented);
        encoder
            .set_exif_metadata(
                b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec(),
            )
            .unwrap();
        encoder
            .write_image(&pixels, 2, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert_ico_pixels(ico_bytes(&[(2, 1, 32, &oriented)]), (2, 1), &pixels);
        let large_pixels = [1, 2, 3, 255].repeat(257);
        let large = ico_png(257, 1, &large_pixels);
        // Directory zero means 256; upstream compares actual.min(256).
        assert_ico_pixels(ico_bytes(&[(0, 1, 32, &large)]), (257, 1), &large_pixels);
        let mut rgb = Vec::new();
        image::codecs::png::PngEncoder::new(&mut rgb)
            .write_image(&[1, 2, 3], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        for invalid in [
            ico_bytes(&[(1, 1, 32, &oriented)]),
            ico_bytes(&[(1, 1, 24, &rgb)]),
        ] {
            let budget = Budget::new(invalid.len(), 8);
            let encoded_length = invalid.len();
            assert!(matches!(
                bounded_handle(invalid, &budget),
                Loaded::Unavailable
            ));
            assert_eq!(budget.encoded.available_permits(), encoded_length);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget.pixels.available_permits(), 8);
        }
        let mut dib = vec![0; 48];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1u32.to_le_bytes());
        dib[8..12].copy_from_slice(&2u32.to_le_bytes()); // Image plus AND mask.
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&4u32.to_le_bytes());
        dib[40..44].copy_from_slice(&[3, 2, 1, 255]);
        dib[44] = 0x80; // Mask the sole pixel transparent.
        let bytes = ico_bytes(&[(1, 1, 32, &dib)]);
        let input = bounded_decoder_input(&bytes).unwrap();
        assert_eq!(input.bytes.as_ptr(), bytes.as_ptr());
        assert_eq!(input.bytes.len(), bytes.len());
        assert_eq!(input.orientation_override, None);
        assert_ico_pixels(bytes, (1, 1), &[1, 2, 3, 0]);
    }
    #[test]
    fn ico_png_metadata_receives_constructor_limits_and_retains_full_input_accounting() {
        use image::{ImageDecoder, ImageEncoder};
        let profile = vec![0; 4096];
        let mut png = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut png);
        encoder.set_icc_profile(profile.clone()).unwrap();
        encoder
            .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(png.len() < 1024);
        let bytes = ico_bytes(&[(1, 1, 32, &png)]);
        let input = bounded_decoder_input(&bytes).unwrap();
        assert_eq!(input.bytes, png);
        for max_alloc in [1024, 16384] {
            let mut decoder = decoder_with_limits(&input, max_alloc).unwrap();
            let metadata = decoder.icc_profile().unwrap();
            // png suppresses an iCCP inflation-limit error and still decodes
            // pixels. Inspect metadata, with the high-limit positive control.
            assert_eq!(metadata, (max_alloc > 4096).then(|| profile.clone()));
            assert_eq!(
                image::DynamicImage::from_decoder(decoder)
                    .unwrap()
                    .into_rgba8()
                    .as_raw(),
                &[1, 2, 3, 255]
            );
        }
        assert_ico_pixels(bytes.clone(), (1, 1), &[1, 2, 3, 255]);
        let budget = Budget::new(png.len(), 4);
        assert!(
            matches!(bounded_handle(bytes.clone(), &budget), Loaded::Deferred(pressure) if pressure.permits as usize == bytes.len())
        );
        assert_eq!(budget.encoded.available_permits(), png.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 4);
    }
    // The following three WebP fixtures come from web-platform-tests at
    // Servo revision 5c14ac9428d8d95c2f6c2f0300fe248d3929f282.
    // # The 3-Clause BSD License
    //
    // Copyright © web-platform-tests contributors
    //
    // Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:
    //
    // 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.
    // 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.
    // 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.
    //
    // THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
    // html/canvas/element/manual/wide-gamut-canvas/resources/pattern-srgb.webp
    const LOSSY_WEBP: &str = concat!(
        "UklGRsgAAABXRUJQVlA4ILwAAAAwBACdASoUABQAAAAAJZAB0gH6gUoC1AP4B7YGQAfoBvAANUF9XYU4RdAA/v//FuB/WH/7",
        "RB//hsjft7//6A7T/4ySu7/ugLf9Y2/lWz/P//Zj5LbXUocuZerf//5T90WfzRF/L1D8Va//vi8Wt/x7Cf6WH+nFylfluYyr",
        "Vg+80P//+Hl9gX+g7x/v4oa+uhv/uHivX/h1vq/6Ae35Yr5fI8BkrTPnLz5+rYfedEzUtJ+cy06x8xCadXgAAA==",
    );
    // html/canvas/element/manual/wide-gamut-canvas/resources/pattern-srgb-transparent.webp
    const ALPHA_WEBP: &str = concat!(
        "UklGRvQAAABXRUJQVlA4WAoAAAAQAAAAEwAAEwAAQUxQSBAAAAABB1DAiAgABOF/3UBE/1MFVlA4IL4AAADwBACdASoUABQA",
        "AAAAJZgB0gH6gfwDBDlAfgBcAH8AvwD9AN4AIgAM3F9XXxj8GAAA/v/+Iir6gP/n3af+gBffxnP/7lnB/+JyuPZ//PpuVPWv",
        "/IjxNWapByUeKzquf//+6ivgt/kX9dev6jGv/zv5ytT/nzaP45/t64Vq+lUyGhcMQo/r///8sl90qavx/oaGFd28f/xNNCan",
        "/Mjf7KrT/hkqs714exbeOwRobPr4ijjHCUdiIXQguaWs+cAA",
    );
    // html/semantics/embedded-content/the-img-element/resources/animated.webp
    const ANIMATED_WEBP: &str = concat!(
        "UklGRhYBAABXRUJQVlA4WAoAAAACAAAAYwAAYwAAQU5JTQYAAAD/////AQBBTk1GdAAAAAAAAAAAAGMAAGMAAGQAAABWUDgg",
        "XAAAAFAGAJ0BKmQAZAA+MRiLRCIhoRBUACADBLS3cLn0uA/AD8AAAOMgYi8sA11N7iLywDXU3uIvLANdTeiAAP7/FJ///7GY",
        "/aM/ySv//4Mx/BmP4Mx/8J8bLcNAAAAAQU5NRm4AAAAAAAAAAABjAABjAAAQJwAAVlA4IFYAAABQBgCdASpkAGQAPjEYi0Qi",
        "IaEQVAAgAwS0t3C59LgPwA/AAADG11N7iLywDXU3uIvLANdTe4i8sA08AAD+/xvOf/+vRB4g360H//3T0hA0f8lAAAAAAA==",
    );
    fn webp_fixture(encoded: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
    }
    fn push_webp_fixture_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if payload.len() & 1 != 0 {
            bytes.push(0);
        }
    }
    fn extended_webp_fixture(chunks: &[u8], animated: bool, alpha: bool) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        let mut canvas = [0u8; 10];
        canvas[0] = u8::from(animated) * 0x02 | u8::from(alpha) * 0x10;
        canvas[4] = 19; // The fixture's VP8 keyframe is 20x20.
        canvas[7] = 19;
        push_webp_fixture_chunk(&mut bytes, b"VP8X", &canvas);
        if animated {
            push_webp_fixture_chunk(&mut bytes, b"ANIM", &[0; 6]);
            let mut frame = vec![0; 16];
            frame[6] = 19;
            frame[9] = 19;
            frame[15] = 0x02; // Replace the canvas, without alpha blending.
            frame.extend_from_slice(chunks);
            push_webp_fixture_chunk(&mut bytes, b"ANMF", &frame);
        } else {
            bytes.extend_from_slice(chunks);
        }
        let length = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }
    fn animated_alpha_webp_fixture() -> Vec<u8> {
        let simple = webp_fixture(LOSSY_WEBP);
        let mut chunks = Vec::new();
        // Uncompressed ALPH: one control byte plus 400 opaque alpha values.
        // Its odd length requires padding before the actual VP8 header.
        let mut alpha = vec![255; 401];
        alpha[0] = 0;
        push_webp_fixture_chunk(&mut chunks, b"ALPH", &alpha);
        chunks.extend_from_slice(&simple[12..]);
        extended_webp_fixture(&chunks, true, true)
    }
    #[test]
    fn webp_preview_preserves_actual_lossy_static_animated_and_alpha_pixels() {
        let simple = webp_fixture(LOSSY_WEBP);
        let opaque = image::load_from_memory(&simple).unwrap().into_rgba8();
        let assert_pixels = |bytes: Vec<u8>, expected: &image::RgbaImage| {
            let budget = Budget::new(bytes.len(), expected.len());
            let encoded_length = bytes.len();
            let Loaded::Ready(iced::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            }) = bounded_handle(bytes, &budget)
            else {
                panic!("valid lossy WebP must retain its pixels");
            };
            assert_eq!((width, height), expected.dimensions());
            assert_eq!(pixels.as_ref(), expected.as_raw());
            assert_eq!(budget.encoded.available_permits(), encoded_length);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            drop(pixels);
            assert_eq!(budget.pixels.available_permits(), expected.len());
        };
        assert_pixels(extended_webp_fixture(&simple[12..], false, false), &opaque);
        assert_pixels(animated_alpha_webp_fixture(), &opaque);
        assert_pixels(simple, &opaque);
        let alpha = webp_fixture(ALPHA_WEBP);
        let expected = image::load_from_memory(&alpha).unwrap().into_rgba8();
        assert_pixels(alpha, &expected);
        let animated = webp_fixture(ANIMATED_WEBP);
        let expected = image::load_from_memory(&animated).unwrap().into_rgba8();
        assert_pixels(animated.clone(), &expected);
        let mut unused_frame = animated;
        let second = unused_frame
            .windows(3)
            .enumerate()
            .filter(|(_, bytes)| *bytes == b"\x9d\x01\x2a")
            .nth(1)
            .unwrap()
            .0;
        unused_frame[second + 3..second + 5].copy_from_slice(&101u16.to_le_bytes());
        assert_pixels(unused_frame, &expected);
    }
    #[test]
    fn webp_preview_refuses_vp8_canvas_and_frame_mismatches_before_decoder_construction() {
        let simple = webp_fixture(LOSSY_WEBP);
        let extended = extended_webp_fixture(&simple[12..], false, false);
        for (mut bytes, height) in [
            (extended, false),
            (webp_fixture(ANIMATED_WEBP), true),
            (animated_alpha_webp_fixture(), false),
        ] {
            let magic = bytes
                .windows(3)
                .position(|bytes| bytes == b"\x9d\x01\x2a")
                .unwrap();
            let offset = magic + if height { 5 } else { 3 };
            let dimension = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
            // Only a one-pixel mismatch: never exercise a large allocation.
            bytes[offset..offset + 2].copy_from_slice(&(dimension + 1).to_le_bytes());
            assert!(bounded_decoder_input(&bytes).is_none());
            let budget = Budget::new(bytes.len(), 100 * 100 * 4);
            let encoded_length = bytes.len();
            assert!(matches!(
                bounded_handle(bytes, &budget),
                Loaded::Unavailable
            ));
            assert_eq!(budget.encoded.available_permits(), encoded_length);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget.pixels.available_permits(), 100 * 100 * 4);
        }
    }
    fn oriented_webp(orientation: u8) -> Vec<u8> {
        use image::ImageEncoder;
        // Raw little-endian TIFF: one SHORT Orientation entry, no JPEG prefix.
        let mut exif = *b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        exif[18] = orientation;
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        encoder.set_exif_metadata(exif.to_vec()).unwrap();
        encoder
            .write_image(
                &[255, 0, 0, 255, 0, 255, 0, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        assert_eq!(&bytes[12..16], b"VP8X");
        bytes
    }
    #[test]
    fn webp_preview_preserves_all_exif_orientations_from_bounded_metadata() {
        for orientation in 1..=8 {
            let bytes = oriented_webp(orientation);
            let budget = Budget::new(bytes.len(), 8);
            let Loaded::Ready(iced::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            }) = bounded_handle(bytes.clone(), &budget)
            else {
                panic!("valid oriented WebP must decode");
            };
            assert_eq!(
                (width, height),
                if orientation >= 5 { (1, 2) } else { (2, 1) },
                "orientation {orientation}"
            );
            let expected = if matches!(orientation, 2 | 3 | 7 | 8) {
                [0, 255, 0, 255, 255, 0, 0, 255]
            } else {
                [255, 0, 0, 255, 0, 255, 0, 255]
            };
            assert_eq!(pixels.as_ref(), &expected, "orientation {orientation}");
            assert_eq!(budget.encoded.available_permits(), bytes.len());
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget.pixels.available_permits(), 0);
            drop(pixels);
            assert_eq!(budget.pixels.available_permits(), 8);
        }
    }
    #[test]
    fn webp_preview_rejects_missing_chunk_bytes_and_releases_every_lease() {
        let bytes = oriented_webp(6);
        let exif = bytes.len() - 26 - 8;
        assert_eq!(&bytes[exif..exif + 4], b"EXIF");
        let mut truncated_exif = bytes.clone();
        // A modest declaration exercises the same truncated-range failure
        // without requesting a large allocation even in the unfixed decoder.
        truncated_exif[exif + 4..exif + 8].copy_from_slice(&4096u32.to_le_bytes());
        let mut long_riff = bytes.clone();
        long_riff[4..8].copy_from_slice(&((bytes.len() - 8 + 2) as u32).to_le_bytes());
        let mut short_riff = bytes.clone();
        short_riff[4..8].copy_from_slice(&3u32.to_le_bytes());
        let mut truncated_header = bytes.clone();
        truncated_header.extend_from_slice(b"JUNK\x01\0\0");
        let length = (truncated_header.len() - 8) as u32;
        truncated_header[4..8].copy_from_slice(&length.to_le_bytes());
        let mut missing_padding = bytes.clone();
        missing_padding.extend_from_slice(b"JUNK\x01\0\0\0x");
        let length = (missing_padding.len() - 8) as u32;
        missing_padding[4..8].copy_from_slice(&length.to_le_bytes());
        let budget = Budget::new(4096, 8);
        for malformed in [
            truncated_exif,
            long_riff,
            short_riff,
            truncated_header,
            missing_padding,
        ] {
            assert!(matches!(
                bounded_handle(malformed, &budget),
                Loaded::Unavailable
            ));
            assert_eq!(budget.encoded.available_permits(), 4096);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget.pixels.available_permits(), 8);
        }
        let Loaded::Ready(handle) = bounded_handle(bytes, &budget) else {
            panic!("valid WebP must still decode after malformed containers");
        };
        drop(handle);
        assert_eq!(budget.encoded.available_permits(), 4096);
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 8);
    }
    #[test]
    fn webp_preview_keeps_first_exif_unknown_padding_and_declared_container_scope() {
        let mut bytes = oriented_webp(6);
        let mut second_exif = bytes[bytes.len() - 26..].to_vec();
        second_exif[18] = 8;
        bytes.extend_from_slice(b"JUNK\x01\0\0\0x\0");
        bytes.extend_from_slice(b"JUNK\0\0\0\0");
        bytes.extend_from_slice(b"EXIF\x1a\0\0\0");
        bytes.extend_from_slice(&second_exif);
        let container_length = bytes.len();
        bytes[4..8].copy_from_slice(&((container_length - 8) as u32).to_le_bytes());
        // This invalid frame is outside RIFF. The pinned decoder's extended
        // scan otherwise reads past the declared end and rejects the image.
        bytes.extend_from_slice(b"ANMF\x01\0\0\0x\0");
        let budget = Budget::new(bytes.len(), 8);
        let Loaded::Ready(iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        }) = bounded_handle(bytes.clone(), &budget)
        else {
            panic!("padded unknown chunks and trailing bytes must preserve valid WebP");
        };
        assert_eq!((width, height), (1, 2));
        assert_eq!(pixels.as_ref(), &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        drop(pixels);
        assert_eq!(budget.pixels.available_permits(), 8);
        let budget = Budget::new(container_length, 8);
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Deferred(pressure) if pressure.permits as usize == bytes.len()
        ));
        assert_eq!(budget.encoded.available_permits(), container_length);
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 8);
    }
    #[test]
    fn webp_preview_without_usable_first_exif_stays_unrotated() {
        let bytes = oriented_webp(6);
        let exif = bytes.len() - 26 - 8;
        let mut absent = bytes[..exif].to_vec();
        absent[20] &= !0x08; // Clear VP8X's EXIF-present flag.
        let length = (absent.len() - 8) as u32;
        absent[4..8].copy_from_slice(&length.to_le_bytes());
        let mut invalid_first = bytes.clone();
        invalid_first[exif + 8 + 18] = 0; // Unrecognized TIFF Orientation.
        invalid_first.extend_from_slice(&bytes[exif..]);
        let length = (invalid_first.len() - 8) as u32;
        invalid_first[4..8].copy_from_slice(&length.to_le_bytes());
        for input in [absent, invalid_first] {
            let budget = Budget::new(input.len(), 8);
            let encoded_length = input.len();
            let Loaded::Ready(iced::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            }) = bounded_handle(input, &budget)
            else {
                panic!("missing or unusable first EXIF must not prevent WebP decoding");
            };
            assert_eq!((width, height), (2, 1));
            assert_eq!(pixels.as_ref(), &[255, 0, 0, 255, 0, 255, 0, 255]);
            assert_eq!(budget.encoded.available_permits(), encoded_length);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
            drop(pixels);
            assert_eq!(budget.pixels.available_permits(), 8);
        }
    }
    #[test]
    fn decoded_preview_preserves_all_pinned_exif_orientations() {
        let mut jpeg = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut jpeg),
            &[
                255, 0, 0, 0, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 0, 255,
            ],
            2,
            3,
            image::ExtendedColorType::Rgb8,
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        for orientation in 1..=8 {
            // APP1 Exif: little-endian TIFF with one SHORT Orientation entry.
            let mut exif =
                *b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
            exif[24] = orientation;
            let mut oriented = jpeg[..2].to_vec();
            oriented.extend_from_slice(&[0xff, 0xe1]);
            oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
            oriented.extend_from_slice(&exif);
            oriented.extend_from_slice(&jpeg[2..]);
            let expected = iced::advanced::graphics::image::load(
                &iced::widget::image::Handle::from_bytes(oriented.clone()),
            )
            .unwrap();
            let Loaded::Ready(iced::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            }) = bounded_handle(oriented.clone(), &Budget::new(1024, 24))
            else {
                panic!("valid oriented JPEG must decode in the worker")
            };
            assert_eq!(
                (width, height),
                expected.dimensions(),
                "orientation {orientation}"
            );
            assert_eq!(
                pixels.as_ref(),
                expected.as_raw().as_ref(),
                "orientation {orientation}"
            );
            let Loaded::Ready(iced::widget::image::Handle::Rgba {
                width,
                height,
                pixels,
                ..
            }) = bounded_handle_size(oriented, &Budget::new(1024, 24), Some(128))
            else {
                panic!("small oriented thumbnail must fit without upscaling")
            };
            assert_eq!((width, height), expected.dimensions());
            assert_eq!(pixels.as_ref(), expected.as_raw().as_ref());
        }
    }
    #[test]
    fn one_shot_image_actions_wait_for_decoder_admission_without_changing_their_input() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        const CHILD: &str = "CAGENTS_IMAGE_ONESHOT_ADMISSION_CHILD";
        const TEST: &str = "app::chat_ui::images::tests::one_shot_image_actions_wait_for_decoder_admission_without_changing_their_input";
        let Some(marker) = std::env::var_os(CHILD) else {
            // This test intentionally blocks the process-global decode budget.
            // Isolate it from other image tests, including parallel test runs.
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("verified");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(CHILD, &marker)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert_eq!(
                        std::fs::read(&marker).unwrap(),
                        b"one-shot image actions verified"
                    );
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("one-shot image action failed to finish after transient decode release");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        fn wait_for_decoder_queue(busy: &mut tokio::sync::OwnedSemaphorePermit, bytes: usize) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while budget().encoded.available_permits() != ENCODED_BUDGET - bytes {
                assert!(
                    Instant::now() < deadline,
                    "the real action must retain its encoded input while waiting for the decoder"
                );
                std::thread::yield_now();
            }
            // A queued acquire_many takes this single released permit toward
            // its full request. Immediate-Defer leaves it available. This
            // proves actual waiting, even if the worker is descheduled just
            // after encoded admission; no timing-based absence claim is used.
            drop(busy.split(1).unwrap());
            while budget().decode.available_permits() != 0 {
                assert!(
                    Instant::now() < deadline,
                    "the user action must queue for transient decoder admission"
                );
                std::thread::yield_now();
            }
            assert_eq!(budget().encoded.available_permits(), ENCODED_BUDGET - bytes);
        }
        fn assert_released() {
            assert_eq!(budget().encoded.available_permits(), ENCODED_BUDGET);
            assert_eq!(budget().decode.available_permits(), DECODE_BUDGET);
            assert_eq!(budget().pixels.available_permits(), PIXEL_BUDGET);
        }
        assert_released();
        for mode in ["attach", "paste", "cancel-paste"] {
            let (ticket, lease) = attachment_draft::admit().unwrap();
            let mut lease = Some(lease);
            let mut busy = budget().block_decoder();
            let (started, input) = mpsc::channel();
            let (finished, result) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let outcome = chat::attachment_assets::with_test_image(|existing| {
                    let bytes = std::fs::read(&existing.path).unwrap();
                    started.send(bytes.len()).unwrap();
                    let result = if mode == "attach" {
                        clipboard_image::stage_file_active(existing.path, u64::MAX, &ticket)
                    } else {
                        let image =
                            clipboard_image::Image::try_from((bytes, "image/png".into())).unwrap();
                        clipboard_image::save_active(image, u64::MAX, &ticket)
                    };
                    if mode == "cancel-paste" {
                        assert_eq!(
                            chat::attachment_assets::list().unwrap().images.len(),
                            1,
                            "a canceled paste must not publish a durable asset"
                        );
                    }
                    result.map(|_| ())
                });
                finished.send(outcome).unwrap();
            });
            let bytes = input.recv_timeout(Duration::from_secs(2)).unwrap();
            wait_for_decoder_queue(&mut busy, bytes);
            assert!(
                matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "{mode} waits instead of reporting a false capacity error"
            );
            if mode == "cancel-paste" {
                drop(lease.take());
            }
            drop(busy);
            let outcome = result.recv_timeout(Duration::from_secs(2)).unwrap();
            if mode == "cancel-paste" {
                assert!(outcome.unwrap_err().contains("cancelled"));
            } else {
                outcome.unwrap();
            }
            worker.join().unwrap();
            drop(lease);
            assert_released();
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.png");
        let bytes = png();
        std::fs::write(&path, &bytes).unwrap();
        let mut busy = budget().block_decoder();
        let selected = path.clone();
        let (finished, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            finished
                .send(runtime.block_on(file_preview(selected, None)))
                .unwrap();
        });
        wait_for_decoder_queue(&mut busy, bytes.len());
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        // The action already read its immutable input. A retry must not read
        // this changed path and silently show a different image.
        std::fs::write(&path, b"replacement is not an image").unwrap();
        drop(busy);
        let handle = result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let iced::widget::image::Handle::Rgba {
            pixels,
            width,
            height,
            ..
        } = &handle
        else {
            panic!("decoded selected image");
        };
        assert_eq!((*width, *height), (1, 1));
        assert_eq!(pixels.as_ref(), &[1, 2, 3, 255]);
        worker.join().unwrap();
        drop(handle);
        assert_released();
        std::fs::write(marker, b"one-shot image actions verified").unwrap();
    }

    #[test]
    fn decoder_metadata_is_admitted_before_construction_and_releases_its_leases() {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder.set_icc_profile(vec![0; 4096]).unwrap();
        encoder
            .write_image(&[1, 2, 3, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(bytes.windows(4).any(|chunk| chunk == b"iCCP"));
        assert!(
            bytes.len() < 4096,
            "the small fixture contains compressed metadata"
        );
        let mut chunk = 8; // Skip the PNG signature, then follow chunk lengths.
        while &bytes[chunk + 4..chunk + 8] != b"IDAT" {
            let length = u32::from_be_bytes(bytes[chunk..chunk + 4].try_into().unwrap()) as usize;
            chunk += length + 12;
        }
        let truncated_metadata = bytes[..chunk].to_vec();
        let budget = Budget::new(bytes.len(), 4);
        for remaining in [0, 16] {
            let busy = budget
                .decode
                .clone()
                .try_acquire_many_owned((DECODE_BUDGET - remaining) as u32)
                .unwrap();
            // Sixteen bytes fit the old 1-pixel-only estimate, but cannot
            // cover this decoder's live metadata. No metadata/RGBA work may
            // bypass the existing transient admission because pixels are tiny.
            assert!(matches!(
                bounded_handle(bytes.clone(), &budget),
                Loaded::Deferred(pressure)
                    if !pressure.retained && pressure.permits as usize <= DECODE_BUDGET
            ));
            // Without admission, even malformed metadata must wait rather
            // than constructing a decoder merely to reject its missing IDAT.
            assert!(matches!(
                bounded_handle(truncated_metadata.clone(), &budget),
                Loaded::Deferred(_)
            ));
            assert_eq!(budget.encoded.available_permits(), bytes.len());
            assert_eq!(budget.pixels.available_permits(), 4);
            assert_eq!(budget.decode.available_permits(), remaining);
            drop(busy);
            assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        }
        assert!(matches!(
            bounded_handle(truncated_metadata, &budget),
            Loaded::Unavailable
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 4);
        let Loaded::Ready(handle) = bounded_handle(bytes.clone(), &budget) else {
            panic!("the admitted tiny image with metadata must still decode");
        };
        let iced::widget::image::Handle::Rgba {
            pixels,
            width,
            height,
            ..
        } = &handle
        else {
            unreachable!()
        };
        assert_eq!((*width, *height), (1, 1));
        assert_eq!(pixels.as_ref(), &[1, 2, 3, 255]);
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 0);
        drop(handle);
        assert_eq!(budget.pixels.available_permits(), 4);

        // Corrupt the IDAT stream after its metadata. Construction is admitted
        // and the later decode error must release every transient/retained lease.
        let mut damaged = bytes.clone();
        damaged[chunk + 8] ^= 0xff;
        assert!(matches!(
            bounded_handle(damaged, &budget),
            Loaded::Unavailable
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 4);
    }

    #[test]
    fn decoded_preview_converts_high_depth_pixels_and_releases_transient_leases() {
        let mut bytes = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut bytes),
            &[65535u16, 0, 0, 65535]
                .into_iter()
                .flat_map(u16::to_ne_bytes)
                .collect::<Vec<_>>(),
            1,
            1,
            image::ExtendedColorType::Rgba16,
            image::ImageFormat::Png,
        )
        .unwrap();
        let budget = Budget::new(bytes.len(), 4);
        let Loaded::Ready(iced::widget::image::Handle::Rgba { pixels, .. }) =
            bounded_handle(bytes.clone(), &budget)
        else {
            panic!("16-bit PNG must produce an already decoded RGBA handle")
        };
        assert_eq!(pixels.as_ref(), &[255, 0, 0, 255]);
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        assert_eq!(budget.pixels.available_permits(), 0);
        drop(pixels);
        assert_eq!(budget.pixels.available_permits(), 4);
        let budget = Budget::new(bytes.len(), 4);
        let busy = budget
            .decode
            .clone()
            .try_acquire_many_owned((DECODE_BUDGET - 15) as u32)
            .unwrap();
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Deferred(_)
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.pixels.available_permits(), 4);
        assert_eq!(budget.decode.available_permits(), 15);
        drop(busy);
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
        let budget = Budget::new(1024, 4);
        assert!(matches!(
            bounded_handle(png()[..40].to_vec(), &budget),
            Loaded::Unavailable
        ));
        assert_eq!(budget.encoded.available_permits(), 1024);
        assert_eq!(budget.pixels.available_permits(), 4);
        assert_eq!(budget.decode.available_permits(), DECODE_BUDGET);
    }
    #[test]
    fn file_preview_rejects_compressed_bytes_over_the_bound() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("too-large.png");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(chat::MAX_INLINE_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(read_file(path.to_str().unwrap()).is_none());
        assert!(matches!(
            bounded_handle(vec![0; chat::MAX_INLINE_IMAGE_BYTES + 1], budget()),
            Loaded::Unavailable
        ));
    }
    #[test]
    fn gallery_navigation_wraps_and_resets_pan_and_zoom() {
        let entry = |id: &str| Entry {
            id: id.into(),
            managed: false,
            path: format!("/fixture/{id}.png"),
            label: id.into(),
            handle: iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        };
        let mut gallery = Gallery {
            entries: vec![entry("a"), entry("b")],
            selected: 0,
            remote: false,
            reset: 0,
        };
        gallery.next(false);
        assert_eq!(gallery.selected, 1);
        assert_eq!(gallery.reset, 1);
        gallery.next(true);
        assert_eq!(gallery.selected, 0);
        assert_eq!(gallery.url().unwrap().as_str(), "file:///fixture/a.png");
        gallery.remote = true;
        assert!(
            gallery.url().is_none(),
            "a remote path must never open a local file URL"
        );
    }
}
