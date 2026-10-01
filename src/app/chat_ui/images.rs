//! Image handles are shared with the transcript's cache. Opening the gallery
//! does not decode base64, read a file, or copy a pixel buffer.
use super::*;
mod canvas;

/// Check the small image header once when an item changes, before the renderer
/// worker decodes it. A compressed-byte limit alone does not bound pixel memory.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
fn displayable(reader: image::ImageReader<impl std::io::BufRead + std::io::Seek>) -> bool {
    reader
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
        .is_some_and(|(width, height)| {
            width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS
        })
}
pub(super) fn file_handle(path: &str) -> Option<iced::widget::image::Handle> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > crate::chat::MAX_INLINE_IMAGE_BYTES as u64 {
        return None;
    }
    displayable(image::ImageReader::new(std::io::BufReader::new(file)))
        .then(|| iced::widget::image::Handle::from_path(path))
}
pub(super) fn bytes_handle(bytes: Vec<u8>) -> Option<iced::widget::image::Handle> {
    if bytes.len() > chat::MAX_INLINE_IMAGE_BYTES {
        return None;
    }
    displayable(image::ImageReader::new(std::io::Cursor::new(&bytes)))
        .then(|| iced::widget::image::Handle::from_bytes(bytes))
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
                "go-previous-symbolic",
                "Previous image (Left)",
                navigation,
                Action::ImageNext(false),
            ),
            design::caption(format!("{} / {}", self.selected + 1, self.entries.len())).into(),
            icon(
                "go-next-symbolic",
                "Next image (Right)",
                navigation,
                Action::ImageNext(true),
            ),
            iced::widget::Space::new().width(Length::Fill).into(),
            widget::button::standard("Fit")
                .on_press(action(Action::ImageFit))
                .into(),
            widget::button::standard("Open externally")
                .on_press_maybe(self.url().map(|_| action(Action::ImageExternal)))
                .into(),
            icon(
                "window-close-symbolic",
                "Close image (Esc)",
                true,
                Action::CloseImage,
            ),
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
            return design::caption(format!("{label} · Preview unavailable")).into();
        };
        let preview = iced::widget::image(handle.clone())
            .width(Length::Fill)
            .height(280)
            .content_fit(iced::ContentFit::Contain);
        widget::column::with_children(vec![
            widget::button::custom(preview)
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
