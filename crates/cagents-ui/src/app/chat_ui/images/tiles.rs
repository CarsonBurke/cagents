//! Pictures shown one after another, such as the screenshots a turn read or
//! the images a prompt attached, are drawn together as a gallery: a grid of
//! tiles of one size, each holding its whole picture, that wraps to the width
//! it is given. A tile's room is fixed before its pixels arrive, so a gallery
//! never moves the conversation as it loads.
use super::*;
use iced::widget::grid::Sizing;

/// The widest a tile is drawn. A narrower column fits more, narrower tiles
/// rather than fewer wide ones, so a gallery's rows keep one height.
pub(in crate::app::chat_ui) const TILE_WIDTH: f32 = 224.0;
/// The height of a tile's picture, whatever its width.
pub(in crate::app::chat_ui) const TILE_HEIGHT: f32 = 168.0;
/// The space between tiles, across and down.
const TILE_GAP: f32 = step::GAP_S;
/// The line under a tile that names its file.
const CAPTION_HEIGHT: f32 = 16.0;
/// The ring a picture's hover and focus highlight draws around it.
pub(in crate::app::chat_ui) const INSET: u16 = 2;

/// Whether `item` is a picture that can share a gallery: one with pixels to
/// show. An image a provider refused, or one whose file was never named,
/// keeps the line that says so.
pub(in crate::app::chat_ui) fn joins(item: &chat::Item) -> bool {
    matches!(&item.content, ItemContent::Image { path, reference, .. }
        if reference.is_some() || !path.is_empty())
}

/// How many of `members`, from the first, are pictures drawn together. One
/// alone is not a gallery, so this is 0 or 1 unless at least two are.
pub(in crate::app::chat_ui) fn run_length(members: impl IntoIterator<Item = bool>) -> usize {
    members.into_iter().take_while(|&member| member).count()
}

/// The file a picture is named for, when it has one: the name a provider
/// gave it, such as the file a Read returned, or the file its path names.
/// An inline picture, or one a prompt pasted, has none.
pub(in crate::app::chat_ui) fn file_name(content: &ItemContent) -> Option<&str> {
    let ItemContent::Image {
        path,
        description,
        reference,
    } = content
    else {
        return None;
    };
    fn named(name: &str) -> Option<&str> {
        let extension = Path::new(name)
            .extension()?
            .to_string_lossy()
            .to_ascii_lowercase();
        let image = matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "avif" | "tif" | "tiff"
        );
        let leaf = name.rsplit('/').next().unwrap_or(name);
        (image && !leaf.is_empty()).then_some(leaf)
    }
    // An inline image the transcript decoded into its private cache is
    // named for its digest, which says nothing to the reader.
    named(description).or_else(|| {
        (reference.is_none()
            && !path.starts_with("data:")
            && !chat::is_cached_inline_image(Path::new(path)))
        .then(|| named(path))
        .flatten()
    })
}

/// What a picture is called where it is shown: its description, else the
/// file it names, else its place among the pictures beside it.
pub(in crate::app::chat_ui) fn label(content: &ItemContent, number: usize) -> String {
    match content {
        ItemContent::Image { description, .. } if !description.is_empty() => description.clone(),
        content => file_name(content).map_or_else(|| format!("Image {number}"), str::to_owned),
    }
}

/// About how tall a gallery of `pictures` is in a column `width` wide, for
/// placing it before it is drawn.
pub(in crate::app::chat_ui) fn height(pictures: &[&chat::Item], width: f32) -> f32 {
    let count = pictures.len();
    let columns = ((width + TILE_GAP) / (TILE_WIDTH + TILE_GAP))
        .ceil()
        .max(1.0) as usize;
    let rows = count.div_ceil(columns.min(count).max(1)) as f32;
    let caption = if captioned(pictures) {
        step::GAP_XS + CAPTION_HEIGHT
    } else {
        0.0
    };
    let tile = TILE_HEIGHT + 2.0 * f32::from(INSET) + caption;
    rows * tile + (rows - 1.0).max(0.0) * TILE_GAP
}

/// Whether a gallery's tiles are captioned: every one keeps a caption line
/// once any is named, so its rows stay one height.
fn captioned(pictures: &[&chat::Item]) -> bool {
    pictures
        .iter()
        .any(|item| file_name(&item.content).is_some())
}

impl Runtime {
    /// Whether `item` is drawn as a tile when pictures beside it are too:
    /// a picture with pixels to show, prepared and not shown as a failure.
    pub(in crate::app::chat_ui) fn tiled(&self, item: &chat::Item) -> bool {
        joins(item)
            && self.presentation_cache().contains_key(&item.id)
            && match self.selected_subagent.as_deref() {
                Some(agent) => self.failure_ui.child_item(agent, &item.id).is_none(),
                None => self.failure_ui.item(&item.id).is_none(),
            }
    }

    /// Whether `item` is a picture that joins a gallery of `role`'s pictures.
    /// A gallery holds one sender's pictures, so the reader's never share one
    /// with an agent's.
    pub(in crate::app::chat_ui) fn tiled_from(&self, item: &chat::Item, role: chat::Role) -> bool {
        item.role == role && self.tiled(item)
    }

    /// `pictures`, one after another in the conversation, as one gallery.
    pub(in crate::app::chat_ui) fn image_tiles(
        &self,
        pictures: &[&chat::Item],
    ) -> Element<'static, Message> {
        let captioned = captioned(pictures);
        let tiles = pictures.iter().enumerate().map(|(position, item)| {
            let tile = self.image_tile(item, position + 1);
            if !captioned {
                return tile;
            }
            let caption = crate::ui_controls::FadeText::caption(
                file_name(&item.content).unwrap_or_default().to_owned(),
            )
            .width(Length::Fill);
            widget::column::with_children(vec![
                tile,
                widget::container(caption)
                    .padding([0, INSET])
                    .height(CAPTION_HEIGHT)
                    .into(),
            ])
            .spacing(step::GAP_XS)
            .into()
        });
        // At most as wide as its tiles side by side, so two pictures are
        // two full tiles rather than two narrow ones in a wide row.
        let count = pictures.len() as f32;
        let widest = count * TILE_WIDTH + (count - 1.0).max(0.0) * TILE_GAP;
        widget::container(
            iced::widget::Grid::with_children(tiles)
                .fluid(TILE_WIDTH)
                .spacing(TILE_GAP)
                .height(Sizing::EvenlyDistribute(Length::Shrink)),
        )
        .max_width(widest)
        .into()
    }

    /// One tile of a gallery: the picture, fit whole within the tile, which
    /// opens it in the viewer, or what stands in for it until it is read.
    fn image_tile(&self, item: &chat::Item, number: usize) -> Element<'static, Message> {
        let label = label(&item.content, number);
        let cached = self.presentation_cache().get(&item.id);
        // `sync` stops every read the transcript no longer wants.
        let loading = cached.is_some_and(|cached| cached.image_source.is_some());
        let (picture, press, tip): (Element<'static, Message>, _, _) =
            match cached.map(|cached| (cached, &cached.image)) {
                Some((_, Some(handle))) => (
                    iced::widget::image(handle.clone())
                        .content_fit(iced::ContentFit::Contain)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    Some(Action::OpenImage(self.target, item.id.clone())),
                    label.clone(),
                ),
                Some((cached, None)) if cached.image_unavailable => (
                    design::caption("Preview unavailable").into(),
                    None,
                    cached.image_error.clone().unwrap_or_else(|| label.clone()),
                ),
                Some((cached, None)) if cached.image_wait.is_some() => (
                    design::caption("Waiting for preview memory…").into(),
                    Some(Action::OpenImage(self.target, item.id.clone())),
                    label.clone(),
                ),
                // Read in a moment; the empty tile already holds its room.
                Some(_) if loading => (
                    widget::Space::new().into(),
                    Some(Action::OpenImage(self.target, item.id.clone())),
                    label.clone(),
                ),
                _ => (
                    design::caption("Load preview").into(),
                    Some(Action::LoadImage(self.target, item.id.clone())),
                    format!("{label} · Load preview"),
                ),
            };
        let frame = widget::container(picture)
            .center(Length::Fill)
            .height(TILE_HEIGHT)
            .clip(true)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_S; 4],
                false,
            ));
        // A tile that loads rather than opens says so to assistive
        // technology as it does on screen.
        let name = if matches!(press, Some(Action::LoadImage(..))) {
            tip.clone()
        } else {
            label
        };
        let tile = crate::interaction::a11y_target(
            widget::button::custom(frame)
                .name(name)
                .class(crate::ui_controls::control_style(false, None))
                .padding(INSET)
                .width(Length::Fill)
                .on_press_maybe(press.map(action)),
        );
        // Above, clear of the caption beneath that it would cover.
        let tile = crate::interaction::tooltip_above(tile, tip);
        match cached.and_then(|cached| cached.image.as_ref()) {
            Some(handle) => {
                let image = ImageRef::Transcript(self.target, handle.id());
                let path = clipboard::has_copyable_path(self, &item.content);
                crate::interaction::context_menu_lazy(tile, move || {
                    clipboard::menu(image, path, false)
                })
            }
            None => tile,
        }
    }

    /// The size the picture `id` names is read at: a tile's when it is drawn
    /// in a gallery, by the rule that draws it, and otherwise a picture
    /// alone's.
    pub(in crate::app::chat_ui) fn image_fit(&self, id: &str) -> Fit {
        if self.image_run(id).len() > 1 {
            Fit::Tile
        } else {
            Fit::Alone
        }
    }

    /// The pictures drawn in one gallery with the one `id` names, in order;
    /// just that one when it is drawn alone.
    pub(in crate::app::chat_ui) fn image_run(&self, id: &str) -> Vec<&Arc<chat::Item>> {
        if self.selected_subagent.is_some() {
            return self.run_around(items(self).iter().collect(), id);
        }
        let all = &self.snapshot.items;
        let Some(index) = all.iter().position(|item| item.id == id) else {
            return Vec::new();
        };
        let Some(turn) = self.turns.iter().find(|turn| {
            turn.prompt.contains(&index) || turn.prose.iter().any(|placed| placed.index == index)
        }) else {
            return vec![&all[index]];
        };
        let side = if turn.prompt.contains(&index) {
            turn.prompt
                .iter()
                .filter_map(|&index| all.get(index))
                .collect()
        } else {
            turn.prose
                .iter()
                .filter_map(|placed| all.get(placed.index))
                .collect()
        };
        self.run_around(side, id)
    }

    /// The pictures of `members`, drawn one after another from one sender,
    /// that include the one `id` names.
    fn run_around<'a>(
        &self,
        members: Vec<&'a Arc<chat::Item>>,
        id: &str,
    ) -> Vec<&'a Arc<chat::Item>> {
        let Some(at) = members.iter().position(|item| item.id == id) else {
            return Vec::new();
        };
        if !self.tiled(members[at]) {
            return vec![members[at]];
        }
        let role = members[at].role;
        let joins = |item: &&Arc<chat::Item>| self.tiled_from(item, role);
        let start = at - run_length(members[..at].iter().rev().map(joins));
        let end = at + run_length(members[at..].iter().map(joins));
        members[start..end].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_is_named_for_its_file_and_otherwise_for_its_place() {
        let named = |path: &str, description: &str, managed: bool| ItemContent::Image {
            path: path.into(),
            description: description.into(),
            reference: managed.then(|| chat::ImageReference {
                id: uuid::Uuid::nil(),
                number: 17,
                device: uuid::Uuid::nil(),
                name: "pasted.png".into(),
                sha256: String::new(),
                extension: "png".into(),
            }),
        };
        for (content, file, named) in [
            (
                named("data:image/png;base64,AAAA", "big-red.png", false),
                Some("big-red.png"),
                "big-red.png",
            ),
            (
                named("/home/me/shots/after.PNG", "", false),
                Some("after.PNG"),
                "after.PNG",
            ),
            (
                named("docs/local/before.webp", "", false),
                Some("before.webp"),
                "before.webp",
            ),
            // A pasted image's description is its place among a prompt's
            // attachments, not a file.
            (
                named("/cache/pasted.png", "Image 17", true),
                None,
                "Image 17",
            ),
            (
                named("data:image/png;base64,AAAA", "", false),
                None,
                "Image 3",
            ),
            (
                named(
                    &format!(
                        "/state/chat-{}/image-{}.png",
                        uuid::Uuid::nil(),
                        "0".repeat(64)
                    ),
                    "",
                    false,
                ),
                None,
                "Image 3",
            ),
            (named("/tmp/render", "", false), None, "Image 3"),
        ] {
            assert_eq!(file_name(&content), file, "{content:?}");
            assert_eq!(label(&content, 3), named, "{content:?}");
        }
    }

    #[test]
    fn a_run_is_the_pictures_from_the_first() {
        assert_eq!(run_length([true, true, false, true]), 2);
        assert_eq!(run_length([false, true]), 0);
        assert_eq!(run_length([true; 7]), 7);
        assert_eq!(run_length(std::iter::empty()), 0);
    }
}
