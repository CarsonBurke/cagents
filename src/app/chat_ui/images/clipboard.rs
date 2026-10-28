//! Image actions shared by transcript previews and the image viewer: the
//! context menu, the viewer's Copy control and Ctrl+C.
//!
//! An action names the exact decoded handle the reader acted on, never an
//! item ID, so a preview replaced or re-decoded in the meantime cannot copy or
//! open pixels that were not clicked (#1563). Copy encodes those displayed
//! pixels (already oriented, at full decoded size) as PNG on an image worker;
//! the UI thread only hands the encoded bytes to the clipboard.
use super::*;
use iced::clipboard::mime::AsMimeTypes;
use iced::advanced::image::Id;
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

/// Encoded PNG offered to the clipboard. Each paste request receives its own
/// copy on the clipboard worker thread.
#[derive(Clone)]
pub(in crate::app) struct Png(Vec<u8>);

impl AsMimeTypes for Png {
    fn available(&self) -> Cow<'static, [String]> {
        Cow::Owned(vec![PNG.to_owned()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<Cow<'static, [u8]>> {
        (mime_type == PNG).then(|| Cow::Owned(self.0.clone()))
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
    "Copying image data needs a Wayland session. Copy the image path or open it externally instead.";

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
    Ok(Png(png))
}

/// Start copying `handle`. A newer copy anywhere in the app supersedes this
/// one, including while it waits for or runs on an image worker.
pub(in crate::app) fn copy(handle: Handle) -> Task<Message> {
    let intent = text_copy::Intent::begin();
    let current = intent.clone();
    cosmic::task::future(async move {
        let result = run_worker(move || {
            current
                .is_current()
                .then(|| encode(&handle))
                .transpose()
        })
        .await
        .and_then(|result| result);
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

/// The image context menu. `path` and `external` say whether those actions
/// apply to this image; Ctrl+C is shown where it works, in the viewer.
pub(in crate::app) fn menu(
    image: ImageRef,
    path: bool,
    external: bool,
) -> Vec<widget::menu::Tree<Message>> {
    use widget::menu::{Item, KeyBind, key_bind::Modifier};
    let command = |command| MenuCommand(image, command);
    let mut items = Vec::with_capacity(4);
    if matches!(image, ImageRef::Transcript(..)) {
        items.push(Item::Button("Open image", None, command(ImageCommand::Open)));
    }
    items.push(if supported() {
        Item::Button("Copy image", None, command(ImageCommand::Copy))
    } else {
        Item::ButtonDisabled("Copy image", None, command(ImageCommand::Copy))
    });
    if path {
        items.push(Item::Button(
            "Copy image path",
            None,
            command(ImageCommand::CopyPath),
        ));
    }
    if external {
        items.push(Item::Button(
            "Open externally",
            None,
            command(ImageCommand::OpenExternally),
        ));
    }
    let mut key_binds = HashMap::new();
    if matches!(image, ImageRef::Viewer(_)) {
        key_binds.insert(
            KeyBind {
                modifiers: vec![Modifier::Ctrl],
                key: iced::keyboard::Key::Character("c".into()),
            },
            command(ImageCommand::Copy),
        );
    }
    widget::menu::items(&key_binds, items)
}

/// A literal image path the reader can copy. Managed assets are excluded
/// (their snapshot path is a non-authority hint), as are inline data and a
/// child's relative path, whose working directory is unknown here.
pub(in crate::app) fn copyable_path(runtime: &Runtime, content: &ItemContent) -> Option<String> {
    let ItemContent::Image {
        path, reference, ..
    } = content
    else {
        return None;
    };
    if reference.is_some() || path.is_empty() || path.starts_with("data:") {
        return None;
    }
    if Path::new(path).is_absolute() {
        return Some(path.clone());
    }
    if runtime.selected_subagent.is_some()
        || Url::parse(path).is_ok_and(|url| !url.scheme().is_empty())
    {
        return None;
    }
    Some(
        runtime
            .session
            .launch
            .recipe
            .working_directory
            .join(path)
            .to_string_lossy()
            .into_owned(),
    )
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
    fn the_viewer_menu_shows_its_copy_shortcut_and_previews_offer_open() {
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let id = Handle::from_rgba(1, 1, vec![0; 4]).id();
        assert_eq!(menu(ImageRef::Transcript(target, id), false, false).len(), 2);
        assert_eq!(menu(ImageRef::Transcript(target, id), true, false).len(), 3);
        assert_eq!(menu(ImageRef::Viewer(id), true, true).len(), 3);
        assert_eq!(menu(ImageRef::Viewer(id), false, false).len(), 1);
    }
}
