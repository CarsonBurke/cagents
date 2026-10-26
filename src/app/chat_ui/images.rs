//! Image handles are shared with the transcript's cache. Opening the gallery
//! does not decode base64, read a file, or copy a pixel buffer.
use super::*;
mod canvas;
pub(super) fn canvas_focus_id() -> widget::Id {
    widget::Id::new("chat.image-viewer.canvas")
}

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

fn decode_handle(bytes: &[u8], budget: &Budget, edge: Option<u32>, max_bytes: usize) -> Loaded {
    if bytes.len() > max_bytes {
        return Loaded::Unavailable;
    }
    let Some((width, height)) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
    else {
        return Loaded::Unavailable;
    };
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || pixels > MAX_PIXELS {
        return Loaded::Unavailable;
    }
    let encoded = match reserve(&budget.encoded, bytes.len() as u32, false) {
        Ok(permit) => permit,
        Err(deferred) => return deferred,
    };
    // Reserve before decode so temporary budget pressure can retry without
    // repeatedly spending the two workers on full-size pixel conversion. The
    // thumbnail never upscales and fits inside edge², even after orientation.
    let retained_pixels = edge.map_or(pixels, |edge| pixels.min(u64::from(edge).pow(2)));
    let mut pixels = match reserve(&budget.pixels, (retained_pixels * 4) as u32, true) {
        Ok(permit) => permit,
        Err(deferred) => return deferred,
    };
    use image::ImageDecoder;
    let mut reader =
        match image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format() {
            Ok(reader) => reader,
            Err(_) => return Loaded::Unavailable,
        };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let Ok(mut decoder) = reader.into_decoder() else {
        return Loaded::Unavailable;
    };
    let decoded_bytes = decoder.total_bytes();
    if decoder.dimensions() != (width, height) || decoded_bytes > MAX_DECODE_BYTES {
        return Loaded::Unavailable;
    }
    // Account for the decoder's output and simultaneous RGBA conversion/rotation
    // buffers. This bounds known pixel buffers across both workers, rather than
    // assuming a four-byte pixel for 16-bit or floating-point source formats.
    let rgba_bytes = u64::from(width) * u64::from(height) * 4;
    let Some(decode_bytes) = decoded_bytes
        .checked_add(rgba_bytes * 2)
        .and_then(|total| u32::try_from(total).ok())
    else {
        return Loaded::Unavailable;
    };
    let _decode = match reserve(&budget.decode, decode_bytes, false) {
        Ok(permit) => permit,
        Err(deferred) => return deferred,
    };
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
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
    load_file_with_request(Path::new(path), origin, request)
}
fn load_file_with_request(
    path: &Path,
    origin: &Origin,
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
        Ok(bytes) => match bounded_handle(bytes, budget()) {
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
    match worker(move || load_file_with_request(&path, &origin, crate::device_service::request))
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
    match decode_handle(
        bytes,
        budget(),
        Some(1),
        chat::attachment_assets::MAX_IMAGE_BYTES as usize,
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
    pub(super) actual_size: bool,
    target: Option<Target>,
    agent: Option<String>,
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
                label: format!(
                    "{} · {}",
                    device.unwrap_or("This device"),
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                handle,
            }],
            selected: 0,
            remote: device.is_some(),
            reset: 0,
            actual_size: false,
            target: None,
            agent: None,
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
                    label: reference
                        .as_ref()
                        .map(|reference| reference.name.clone())
                        .filter(|name| !name.is_empty())
                        .or_else(|| {
                            Path::new(path)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(str::to_owned)
                        })
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| {
                            if description.is_empty() {
                                "Image".into()
                            } else {
                                description.clone()
                            }
                        }),
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
            actual_size: false,
            target: Some(runtime.target),
            agent: runtime.selected_subagent.clone(),
        })
    }
    /// Device-file previews retain their dialog; transcript previews belong to
    /// the exact runtime generation and agent that supplied the clicked image.
    pub(in crate::app) fn is_modal(&self) -> bool {
        self.target.is_none()
    }
    #[cfg(test)]
    pub(super) fn selected_handle_id(&self) -> iced::advanced::image::Id {
        self.entries[self.selected].handle.id()
    }
    pub(in crate::app) fn target(&self) -> Option<Target> {
        self.target
    }
    pub(super) fn belongs_to(&self, runtime: &Runtime) -> bool {
        self.target == Some(runtime.target) && self.agent == runtime.selected_subagent
    }
    pub(super) fn select(&mut self, index: usize) {
        if index < self.entries.len() && index != self.selected {
            self.selected = index;
            self.reset = self.reset.wrapping_add(1);
        }
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
        let mut controls = Vec::new();
        if self.entries.len() > 1 {
            controls.extend([
                icon(
                    Icon::ChevronLeft,
                    "Previous image (Left)",
                    true,
                    Action::ImageNext(false),
                ),
                design::caption(format!("{} / {}", self.selected + 1, self.entries.len())).into(),
                icon(
                    Icon::ChevronRight,
                    "Next image (Right)",
                    true,
                    Action::ImageNext(true),
                ),
            ]);
        }
        controls.extend([
            crate::ui_controls::button::secondary("Fit")
                .on_press(action(Action::ImageFit))
                .into(),
            crate::ui_controls::button::secondary("100%")
                .on_press(action(Action::ImageActualSize))
                .into(),
            crate::interaction::tooltip(
                crate::ui_controls::button::secondary("Open externally")
                    .on_press_maybe(self.url().map(|_| action(Action::ImageExternal))),
                if self.url().is_some() {
                    "Open the original image in another application"
                } else {
                    "The original image is unavailable to external applications on this device"
                },
            ),
            icon(Icon::Close, "Close image (Esc)", true, Action::CloseImage),
        ]);
        let toolbar = widget::flex_row(controls)
            .spacing(step::GAP_XS as u16)
            .align_items(iced::Alignment::Center);
        let viewer: Element<'_, Message> = iced::widget::image::Viewer::new(entry.handle.clone())
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(if self.actual_size {
                iced::ContentFit::None
            } else {
                iced::ContentFit::Contain
            })
            .min_scale(0.1)
            .max_scale(20.0)
            .into();
        let mut content = vec![
            design::caption(entry.label.as_str()).into(),
            toolbar.into(),
            canvas::resettable(self.reset, viewer),
        ];
        if self.entries.len() > 1 {
            // Reuse already admitted handles. Keep the strip small even if a
            // conversation has many images; opening it never starts a decode.
            let start = self
                .selected
                .saturating_sub(3)
                .min(self.entries.len().saturating_sub(7));
            let thumbs = self
                .entries
                .iter()
                .enumerate()
                .skip(start)
                .take(7)
                .map(|(index, entry)| {
                    widget::button::custom(
                        iced::widget::image(entry.handle.clone())
                            .width(56)
                            .height(40)
                            .content_fit(iced::ContentFit::Contain),
                    )
                    .class(crate::ui_controls::control_style(
                        index == self.selected,
                        None,
                    ))
                    .padding(2)
                    .name(format!(
                        "View {} · image {} of {}",
                        entry.label,
                        index + 1,
                        self.entries.len()
                    ))
                    .selected(index == self.selected)
                    .on_press(action(Action::ImageSelect(index)))
                    .into()
                })
                .collect::<Vec<Element<'_, Message>>>();
            content.push(widget::flex_row(thumbs).spacing(step::GAP_XS as u16).into());
        }
        let dimensions = match &entry.handle {
            iced::widget::image::Handle::Rgba { width, height, .. } => {
                format!("{width} × {height} · ")
            }
            _ => String::new(),
        };
        content.push(
            design::caption(format!("{dimensions}Scroll or + / − to zoom · Drag to pan")).into(),
        );
        let surface = crate::interaction::focused_key(
            widget::container(
                widget::column::with_children(content)
                    .spacing(step::GAP_XS)
                    .height(Length::Fill),
            )
            .padding(step::GAP_S)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .width(Length::Fill)
            .height(Length::Fill),
            |event| {
                let iced::keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
                    return None;
                };
                if !modifiers.is_empty() {
                    return None;
                }
                let command = match key {
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape) => {
                        Action::CloseImage
                    }
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowLeft) => {
                        Action::ImageNext(false)
                    }
                    iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowRight) => {
                        Action::ImageNext(true)
                    }
                    iced::keyboard::Key::Character(key) if key == "0" => Action::ImageFit,
                    iced::keyboard::Key::Character(key) if key == "1" => Action::ImageActualSize,
                    _ => return None,
                };
                Some(action(command))
            },
        );
        if self.is_modal() {
            dialog_ui::overlay(
                dialog_ui::surface(surface, f32::MAX),
                Some(action(Action::CloseImage)),
                1.0,
            )
        } else {
            surface
        }
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
            let placeholder: Element<'static, Message> = if cached.image_unavailable {
                let notice = design::caption(
                    cached
                        .image_error
                        .clone()
                        .unwrap_or_else(|| format!("{label} · Preview unavailable")),
                );
                let can_retry =
                    matches!(&item.content, ItemContent::Image { path, .. } if !path.is_empty());
                widget::column::with_children(std::iter::once(notice.into()).chain(can_retry.then(
                    || {
                        crate::ui_controls::button::secondary("Retry preview")
                            .on_press(action(Action::LoadImage(self.target, item.id.clone())))
                            .into()
                    },
                )))
                .spacing(step::GAP_XS)
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
                    design::caption(label.clone()).into(),
                    crate::ui_controls::button::secondary("Load preview")
                        .on_press(action(Action::LoadImage(self.target, item.id.clone())))
                        .into(),
                ])
                .spacing(step::GAP_XS)
                .into()
            };
            return widget::container(
                widget::column::with_children(vec![
                    widget::container(placeholder)
                        .height(216)
                        .width(Length::Fill)
                        .center_y(Length::Fill)
                        .padding(step::GAP_S)
                        .class(design::surface(
                            Some(design::raised),
                            [design::RADIUS_M; 4],
                            true,
                        ))
                        .into(),
                    design::caption(label).into(),
                ])
                .spacing(step::GAP_XS)
                .width(Length::Fill),
            )
            .width(Length::Fill)
            .max_width(480)
            .into();
        };
        let preview = iced::widget::image(handle.clone())
            .width(Length::Fill)
            .height(216)
            .content_fit(iced::ContentFit::Contain);
        widget::container(
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
            .width(Length::Fill),
        )
        .max_width(480)
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
        let Loaded::Ready(handle) = load_file_with_request(&path, &origin, |operation| {
            assert!(
                matches!(operation, crate::device_service::Operation::ReadImage { root, path: sent }
                if root == Path::new("/") && sent == path)
            );
            Ok(crate::device_service::Reply::Image(remote))
        }) else {
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
        let mut budget = Budget::new(bytes.len(), 4);
        budget.decode = Arc::new(tokio::sync::Semaphore::new(15));
        assert!(matches!(
            bounded_handle(bytes.clone(), &budget),
            Loaded::Deferred(_)
        ));
        assert_eq!(budget.encoded.available_permits(), bytes.len());
        assert_eq!(budget.pixels.available_permits(), 4);
        assert_eq!(budget.decode.available_permits(), 15);
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
            actual_size: false,
            target: None,
            agent: None,
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
