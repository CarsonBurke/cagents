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
    Deferred(Pressure),
}
pub(super) fn bounded_handle(bytes: Vec<u8>, budget: &Budget) -> Loaded {
    bounded_handle_size(bytes, budget, None)
}
fn bounded_handle_size(bytes: Vec<u8>, budget: &Budget, edge: Option<u32>) -> Loaded {
    if bytes.len() > chat::MAX_INLINE_IMAGE_BYTES {
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
fn load(item: &chat::Item) -> Loaded {
    let ItemContent::Image { path, .. } = &item.content else {
        return Loaded::Unavailable;
    };
    let bytes = if Path::new(path).is_absolute() {
        read_file(path)
    } else if path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES && path.starts_with("data:image/") {
        path.split_once(";base64,").and_then(|(_, encoded)| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
    } else {
        None
    };
    bytes.map_or(Loaded::Unavailable, |bytes| bounded_handle(bytes, budget()))
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
    item: Arc<chat::Item>,
    attempt: u64,
    wait: Option<Pressure>,
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
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
        let loaded = worker(move || load(&item)).await;
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
    .unwrap_or(Loaded::Unavailable)
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
    for (id, cached) in &mut runtime.cache {
        if cached.image_item.is_none() && cached.image_source.is_none() && cached.image.is_none() {
            continue;
        }
        if !interested.contains(id) {
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
    if !runtime.cache.contains_key(id) {
        return;
    }
    runtime.preview_request = Some(id.to_owned());
    if let Some(cached) = runtime.cache.get_mut(id) {
        cached.image_item = Some(item.clone());
        if cached.image.is_none() {
            cached.image_source = Some(item);
        }
        cached.image_attempt = cached.image_attempt.wrapping_add(1);
        cached.image_wait = None;
        cached.image_evicted = false;
        cached.image_unavailable = false;
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
        if let Some(cached) = runtime.cache.get_mut(id)
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
    let Some(cached) = runtime.cache.get(&item.id) else {
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
                if let Some(cached) = runtime.cache.get_mut(id)
                    && cached.image.is_some()
                {
                    missing = missing.saturating_sub(pixel_bytes(cached));
                    retire(cached, true);
                    evicted = true;
                }
            }
        }
        let cached = runtime.cache.get_mut(&item.id).unwrap();
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
    let cached = runtime.cache.get_mut(&item.id).unwrap();
    cached.image_source = None;
    cached.image_wait = None;
    cached.image_unavailable = matches!(loaded, Loaded::Unavailable);
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
            let cached = runtime.cache.get(&item.id)?;
            let source = cached.image_source.as_ref()?;
            Some(Subscription::run_with(
                Source {
                    target: runtime.target,
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
    path: String,
    label: String,
    handle: iced::widget::image::Handle,
}
pub(in crate::app) struct Gallery {
    entries: Vec<Entry>,
    selected: usize,
    pub(super) reset: u64,
}
impl Gallery {
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
                let ItemContent::Image { path, description } = &item.content else {
                    return None;
                };
                Some(Entry {
                    id: item.id.clone(),
                    path: path.clone(),
                    label: if description.is_empty() {
                        "Image".into()
                    } else {
                        description.clone()
                    },
                    handle: runtime.cache.get(&item.id)?.image.clone()?,
                })
            })
            .collect();
        let selected = entries.iter().position(|entry| entry.id == id)?;
        Some(Self {
            entries,
            selected,
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
        let path = &self.entries[self.selected].path;
        if Path::new(path).is_absolute() {
            Url::from_file_path(path).ok()
        } else {
            Url::parse(path)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
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
        let caption = |suffix:&str| -> Element<'static,Message> {
            widget::row::with_children(vec![
                find::text(self.target,&item.id,&[9,0],label.clone(),design::TEXT_CAPTION,iced::Font::default(),self.find_paint.clone()),
                design::caption(suffix.to_owned()).into(),
            ]).spacing(step::GAP_XS).into()
        };
        let Some(handle) = &cached.image else {
            return if cached.image_unavailable {
                caption("· Preview unavailable")
            } else if cached.image_wait.is_some() {
                caption("· Waiting for preview memory…")
            } else if cached.image_source.is_some()
                && interest(self)
                    .iter()
                    .any(|candidate| candidate.id == item.id)
            {
                caption("· Loading preview…")
            } else {
                widget::column::with_children(vec![
                    caption(""),
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
            caption("· Click to enlarge"),
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
            path: format!("/fixture/{id}.png"),
            label: id.into(),
            handle: iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        };
        let mut gallery = Gallery {
            entries: vec![entry("a"), entry("b")],
            selected: 0,
            reset: 0,
        };
        gallery.next(false);
        assert_eq!(gallery.selected, 1);
        assert_eq!(gallery.reset, 1);
        gallery.next(true);
        assert_eq!(gallery.selected, 0);
        assert_eq!(gallery.url().unwrap().as_str(), "file:///fixture/a.png");
    }
}
