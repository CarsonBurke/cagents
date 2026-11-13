//! Image actions shared by transcript previews and the image viewer: the
//! context menu, the viewer's Copy control and Ctrl+C.
//!
//! An action names the exact decoded handle the reader acted on, never an
//! item ID, so a preview replaced or re-decoded in the meantime cannot copy or
//! open pixels that were not clicked (#1563). Copy encodes those displayed
//! pixels (already oriented, at full decoded size) as PNG on an image worker;
//! the UI thread only hands the encoded bytes to the clipboard.
use super::*;
use iced::advanced::image::Id;
use iced::clipboard::mime::AsMimeTypes;
use iced::widget::image::Handle;

const PNG: &str = "image/png";

/// Which displayed image an action refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::app) enum ImageRef {
    /// A transcript preview of the conversation `Target` shows.
    Transcript(Target, Id),
    /// An entry of the open image viewer.
    Viewer(Id),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::app) enum ImageCommand {
    Open,
    Copy,
    CopyPath,
    OpenExternally,
}

/// What an [`ImageRef`] currently resolves to.
pub(in crate::app) struct Resolved {
    /// The transcript item, for a preview that opens the viewer.
    pub(in crate::app) item: Option<String>,
    pub(in crate::app) handle: Handle,
    /// A literal file path the reader may copy; never a managed asset hint.
    pub(in crate::app) path: Option<String>,
    pub(in crate::app) url: Option<Url>,
}

/// Encoded PNG offered to the clipboard, shared rather than copied as it moves
/// through messages. Each paste request receives its own copy on the clipboard
/// worker thread.
#[derive(Clone)]
pub(in crate::app) struct Png(std::sync::Arc<[u8]>);

impl AsMimeTypes for Png {
    fn available(&self) -> Cow<'static, [String]> {
        Cow::Owned(vec![PNG.to_owned()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<Cow<'static, [u8]>> {
        (mime_type == PNG).then(|| Cow::Owned(self.0.to_vec()))
    }
}

/// The pinned X11 clipboard backend accepts only text and silently drops
/// image data, so do not claim a copy there. An unknown backend (tests,
/// before the first window) is treated as capable.
pub(in crate::app) fn supported() -> bool {
    use cosmic::app::cosmic::WindowingSystem;
    !matches!(
        cosmic::app::cosmic::windowing_system(),
        Some(WindowingSystem::Xlib | WindowingSystem::Xcb)
    )
}

pub(in crate::app) const UNSUPPORTED: &str =
    "Copying image data needs a Wayland session: the X11 clipboard here accepts only text.";

fn encode(handle: &Handle) -> Result<Png, String> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return Err("The image has no decoded pixels to copy.".into());
    };
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(pixels, *width, *height, image::ExtendedColorType::Rgba8)
        .map_err(|error| format!("Could not encode the image as PNG: {error}"))?;
    Ok(Png(png.into()))
}

/// Encoder scratch and output are admitted against the decode budget at the
/// size of the raw pixels (fast PNG output rarely exceeds it), so concurrent
/// copies of large images wait for memory rather than adding to it.
fn encode_admission(handle: &Handle) -> u32 {
    match handle {
        Handle::Rgba { pixels, .. } => u32::try_from(pixels.len())
            .map_or(DECODE_BUDGET as u32, |bytes| {
                bytes.min(DECODE_BUDGET as u32)
            }),
        _ => 0,
    }
}

/// Start copying `handle`. A newer copy anywhere in the app supersedes this
/// one, including while it waits for memory or an image worker.
pub(in crate::app) fn copy(handle: Handle) -> Task<Message> {
    let intent = text_copy::Intent::begin();
    let current = intent.clone();
    cosmic::task::future(async move {
        let permits = encode_admission(&handle);
        let result = match budget().decode.clone().acquire_many_owned(permits).await {
            Ok(memory) => run_worker(move || {
                let _memory = memory;
                current.is_current().then(|| encode(&handle)).transpose()
            })
            .await
            .and_then(|result| result),
            Err(_) => Err("Image memory admission is closed.".into()),
        };
        action(Action::ImageCopied(intent, result))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MenuCommand(ImageRef, ImageCommand);

impl widget::menu::Action for MenuCommand {
    type Message = Message;

    fn message(&self) -> Message {
        action(Action::Image(self.0, self.1))
    }
}

/// One context menu entry: its label and command.
type MenuEntry = (&'static str, ImageCommand);

/// The image context menu's entries. `path` and `external` say whether those
/// actions apply to this image. Copy image is always offered: where the
/// clipboard cannot hold image data, choosing it says why, which a disabled
/// item cannot.
fn entries(image: ImageRef, path: bool, external: bool) -> Vec<MenuEntry> {
    let mut entries = Vec::with_capacity(4);
    if matches!(image, ImageRef::Transcript(..)) {
        entries.push(("Open image", ImageCommand::Open));
    }
    entries.push(("Copy image", ImageCommand::Copy));
    if path {
        entries.push(("Copy image path", ImageCommand::CopyPath));
    }
    if external {
        entries.push(("Open externally", ImageCommand::OpenExternally));
    }
    entries
}

/// The image context menu. Ctrl+C is shown where it works: in the viewer.
pub(in crate::app) fn menu(
    image: ImageRef,
    path: bool,
    external: bool,
) -> Vec<widget::menu::Tree<Message>> {
    use widget::menu::{Item, KeyBind, key_bind::Modifier};
    let items = entries(image, path, external)
        .into_iter()
        .map(|(label, command)| Item::Button(label, None, MenuCommand(image, command)))
        .collect();
    let mut key_binds = HashMap::new();
    if matches!(image, ImageRef::Viewer(_)) {
        key_binds.insert(
            KeyBind {
                modifiers: vec![Modifier::Ctrl],
                key: iced::keyboard::Key::Character("c".into()),
            },
            MenuCommand(image, ImageCommand::Copy),
        );
    }
    widget::menu::items(&key_binds, items)
}

/// The file an image literally names: an absolute path, a `file://` URL, or a
/// path relative to the conversation's working directory. Managed assets are
/// excluded (their snapshot path is a non-authority hint), as are inline data,
/// other URLs, cached inline images, and a child's relative path, whose
/// working directory is unknown here.
fn literal_path(runtime: &Runtime, content: &ItemContent) -> Option<PathBuf> {
    let ItemContent::Image {
        path, reference, ..
    } = content
    else {
        return None;
    };
    if reference.is_some() || path.is_empty() || path.starts_with("data:") {
        return None;
    }
    if let Some(path) = local_file(path) {
        return Some(path);
    }
    // A relative path names a file in the conversation's worktree; a subagent
    // transcript's worktree is not the one the session launched in.
    (runtime.selected_subagent.is_none()
        && Path::new(path).is_relative()
        && !Url::parse(path).is_ok_and(|url| !url.scheme().is_empty()))
    .then(|| runtime.session.launch.recipe.working_directory.join(path))
}

/// The local file an absolute path or `file://` URL names, if it is one worth
/// offering. A provider's inline image decoded into the private transcript
/// cache is gone with the conversation, so only its pixels are worth copying.
pub(in crate::app) fn local_file(path: &str) -> Option<PathBuf> {
    let path = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Url::parse(path)
            .ok()
            .filter(|url| url.scheme() == "file")?
            .to_file_path()
            .ok()?
    };
    (!crate::chat::is_cached_inline_image(&path)).then_some(path)
}

/// Whether a preview offers Copy image path; checked on every view.
pub(in crate::app) fn has_copyable_path(runtime: &Runtime, content: &ItemContent) -> bool {
    literal_path(runtime, content).is_some()
}

/// The image path Copy image path writes.
pub(in crate::app) fn copyable_path(runtime: &Runtime, content: &ItemContent) -> Option<String> {
    literal_path(runtime, content).map(|path| path.to_string_lossy().into_owned())
}

/// The transcript preview currently drawn with `image`, if it still is.
pub(in crate::app) fn resolve(runtime: &Runtime, image: Id) -> Option<Resolved> {
    let (id, handle) = runtime
        .presentation_cache()
        .iter()
        .find_map(|(id, cached)| {
            cached
                .image
                .as_ref()
                .filter(|handle| handle.id() == image)
                .map(|handle| (id, handle))
        })?;
    let item = items(runtime).iter().find(|item| item.id == *id)?;
    Some(Resolved {
        item: Some(id.clone()),
        handle: handle.clone(),
        path: copyable_path(runtime, &item.content),
        url: None,
    })
}

impl Cagents {
    pub(in crate::app) fn image_command(
        &mut self,
        image: ImageRef,
        command: ImageCommand,
    ) -> Task<Message> {
        let resolved = match image {
            ImageRef::Viewer(id) => self
                .chat_ui
                .image_viewer
                .as_ref()
                .and_then(|gallery| gallery.resolve(id)),
            ImageRef::Transcript(target, id) => self
                .chat_runtime_mut(target)
                .and_then(|runtime| resolve(runtime, id)),
        };
        let Some(resolved) = resolved else {
            self.status = Some(TransientNotice::warning(
                "That image is no longer shown. Open it again to use it.",
            ));
            return Task::none();
        };
        match command {
            ImageCommand::Open => match (image, resolved.item) {
                (ImageRef::Transcript(target, _), Some(item)) => {
                    self.handle_chat_ui(Action::OpenImage(target, item))
                }
                _ => Task::none(),
            },
            ImageCommand::Copy if !supported() => {
                self.status = Some(TransientNotice::warning(UNSUPPORTED));
                Task::none()
            }
            ImageCommand::Copy => copy(resolved.handle),
            ImageCommand::CopyPath => match resolved.path {
                Some(path) => {
                    text_copy::invalidate();
                    self.status = Some(TransientNotice::success("Copied image path"));
                    iced::clipboard::write(path).map(cosmic::Action::App)
                }
                None => Task::none(),
            },
            ImageCommand::OpenExternally => match resolved.url {
                Some(url) => cosmic::task::future(async move {
                    Message::ExternalLinkOpened(open_external(url).await)
                }),
                None => Task::none(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png: &Png) -> image::RgbaImage {
        assert_eq!(png.available().as_ref(), [PNG]);
        let bytes = png.as_bytes(PNG).unwrap();
        assert!(png.as_bytes("text/plain").is_none());
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8()
    }

    #[test]
    fn copy_encodes_the_displayed_pixels_losslessly_as_png() {
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 128, //
            0, 0, 255, 0, 10, 20, 30, 255, //
            1, 2, 3, 4, 250, 251, 252, 253,
        ];
        let handle = Handle::from_rgba(2, 3, pixels.clone());
        let decoded = decode(&encode(&handle).unwrap());
        assert_eq!(decoded.dimensions(), (2, 3));
        assert_eq!(decoded.into_raw(), pixels, "alpha and colour survive");
    }

    #[test]
    fn a_handle_without_decoded_pixels_is_refused() {
        let error = encode(&Handle::from_path("/no/pixels.png")).err().unwrap();
        assert!(error.contains("no decoded pixels"), "{error}");
    }

    #[test]
    fn a_large_image_is_encoded_by_an_image_worker_and_reported_back() {
        let _context = text_copy::isolated_context();
        // Large enough to exercise multi-megabyte buffers in an unoptimized test build.
        let (width, height) = (2048usize, 1024usize);
        let mut pixels = vec![0u8; width * height * 4];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index % 251) as u8, (index / width) as u8, 7, 255]);
        }
        let handle = Handle::from_rgba(width as u32, height as u32, pixels);
        // Building the task does no encoding; the worker does it when polled.
        let task = copy(handle);
        let message = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use iced::futures::StreamExt;
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                match stream.next().await {
                    Some(iced::runtime::Action::Output(message)) => message,
                    _ => panic!("the copy task reports its result"),
                }
            });
        let cosmic::Action::App(Message::ChatUi(Action::ImageCopied(intent, Ok(Some(png))))) =
            message
        else {
            panic!("a current copy produces PNG data");
        };
        assert!(intent.is_current());
        let decoded = decode(&png);
        assert_eq!(decoded.dimensions(), (2048, 1024));
        let last = width * height - 1;
        assert_eq!(
            decoded.get_pixel(2047, 1023).0,
            [(last % 251) as u8, (last / width) as u8, 7, 255]
        );
    }

    #[test]
    fn a_newer_copy_supersedes_an_encode_that_has_not_started() {
        let _context = text_copy::isolated_context();
        let task = copy(Handle::from_rgba(1, 1, vec![1, 2, 3, 255]));
        text_copy::invalidate();
        let message = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use iced::futures::StreamExt;
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                match stream.next().await {
                    Some(iced::runtime::Action::Output(message)) => message,
                    _ => panic!("the copy task reports its result"),
                }
            });
        let cosmic::Action::App(Message::ChatUi(Action::ImageCopied(intent, Ok(None)))) = message
        else {
            panic!("a superseded copy encodes nothing");
        };
        assert!(!intent.is_current());
    }

    #[test]
    fn previews_and_the_viewer_offer_the_actions_that_apply() {
        use ImageCommand::*;
        use widget::menu::Action as _;
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let id = Handle::from_rgba(1, 1, vec![0; 4]).id();
        let preview = ImageRef::Transcript(target, id);
        let viewer = ImageRef::Viewer(id);
        assert_eq!(
            entries(preview, true, false),
            [
                ("Open image", Open),
                ("Copy image", Copy),
                ("Copy image path", CopyPath),
            ]
        );
        assert_eq!(
            entries(viewer, false, true),
            [("Copy image", Copy), ("Open externally", OpenExternally)]
        );
        // Each menu item sends its own command for the same exact image.
        for (_, command) in entries(viewer, true, true) {
            assert!(matches!(
                MenuCommand(viewer, command).message(),
                Message::ChatUi(Action::Image(image, sent)) if image == viewer && sent == command
            ));
        }
        assert_eq!(menu(viewer, true, true).len(), 3);
    }

    #[test]
    fn encoding_memory_is_admitted_at_the_raw_pixel_size() {
        assert_eq!(encode_admission(&Handle::from_rgba(3, 2, vec![0; 24])), 24);
        assert_eq!(encode_admission(&Handle::from_path("/no/pixels.png")), 0);
    }

    #[test]
    fn a_copy_superseded_after_encoding_never_reaches_the_clipboard() {
        let _context = text_copy::isolated_context();
        let intent = text_copy::Intent::begin();
        let task = intent.write_data(Png(vec![1, 2, 3].into()));
        text_copy::invalidate();
        let effects = iced::futures::executor::block_on(async {
            use iced::futures::StreamExt;
            iced::runtime::task::into_stream(task)
                .unwrap()
                .collect::<Vec<_>>()
                .await
        });
        assert!(effects.is_empty(), "the gate runs when the write is polled");
    }
}
