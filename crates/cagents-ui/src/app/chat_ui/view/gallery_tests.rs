//! Pictures one after another in a turn are drawn as one gallery of
//! uniform tiles (#3153).
use super::*;
use iced::advanced::{
    Layout, layout,
    widget::{Operation, Tree, operation::Focusable},
};
use iced::{Rectangle, Size};

fn item(id: &str, role: chat::Role, content: ItemContent) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        content,
        started_at: None,
        at: None,
    }
}

fn read(id: &str, file: &str) -> chat::Item {
    item(
        id,
        chat::Role::Assistant,
        ItemContent::Command {
            label: format!("/project/docs/{file}"),
            command: format!("/project/docs/{file}"),
            working_directory: String::new(),
            action: chat::CommandAction::Read,
            status: chat::ToolStatus::Succeeded,
            exit_code: None,
            duration_ms: Some(10),
            output: String::new(),
        },
    )
}

/// The picture a Read returned, named for its file as Claude's adapter
/// names it.
fn picture(id: &str, role: chat::Role, description: &str) -> chat::Item {
    item(
        id,
        role,
        ItemContent::Image {
            path: "data:image/png;base64,AAAA".into(),
            description: description.into(),
            reference: None,
        },
    )
}

fn said(id: &str, role: chat::Role, text: &str) -> chat::Item {
    item(
        id,
        role,
        ItemContent::Text {
            text: text.into(),
            thinking: false,
            phase: Default::default(),
        },
    )
}

/// Pixels of each picture's own shape, as decoding would leave them.
fn decoded(runtime: &mut Runtime, shapes: &[(u32, u32)]) {
    let ids: Vec<_> = runtime
        .snapshot
        .items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::Image { .. }))
        .map(|item| item.id.clone())
        .collect();
    for (id, &(width, height)) in ids.iter().zip(shapes.iter().cycle()) {
        let cached = runtime.cache.get_mut(id).unwrap();
        cached.image = Some(iced::widget::image::Handle::from_rgba(
            width,
            height,
            [40, 80, 120, 255].repeat((width * height) as usize),
        ));
        cached.image_source = None;
    }
}

fn parts(runtime: &Runtime) -> Vec<Vec<TurnPart>> {
    runtime
        .turns
        .iter()
        .map(|turn| {
            let mut parts = Vec::new();
            runtime.for_each_turn_part(turn, |placement| {
                if let Placement::Row(part, _) = placement {
                    parts.push(part);
                }
            });
            parts
        })
        .collect()
}

#[test]
fn two_reads_of_screenshots_share_one_gallery_beneath_their_strip() {
    use chat::Role::{Assistant, User};
    let (mut runtime, _handle) = frame_cost::live_runtime(&[
        said("ask", User, "Compare the proposed layouts."),
        read("read-slow", "proposed-slow.png"),
        read("read-fast", "proposed-fast.png"),
        picture("read-slow:image:0", Assistant, "proposed-slow.png"),
        // A Read that comes after the first picture still joins the group,
        // and its picture the gallery.
        read("read-other", "proposed-other.png"),
        picture("read-fast:image:0", Assistant, "proposed-fast.png"),
        picture("read-other:image:0", Assistant, "proposed-other.png"),
        said(
            "answer",
            Assistant,
            "The fast layout keeps the inspector visible.",
        ),
        said("mine", User, "Here are mine:"),
        picture("pasted-1", User, "Image 1"),
        picture("pasted-2", User, "Image 2"),
        said("then", Assistant, "And one alone:"),
        read("read-narrow", "narrow.png"),
        picture("read-narrow:image:0", Assistant, "narrow.png"),
    ]);
    decoded(&mut runtime, &[(1568, 1045)]);
    assert_eq!(
        parts(&runtime),
        [
            vec![
                TurnPart::Prompt(0),
                TurnPart::Steps,
                TurnPart::Gallery {
                    prompt: false,
                    start: 0,
                    len: 3
                },
                TurnPart::Prose(3),
            ],
            vec![
                TurnPart::Prompt(8),
                TurnPart::Gallery {
                    prompt: true,
                    start: 1,
                    len: 2
                },
                TurnPart::Prose(0),
            ],
            vec![TurnPart::Steps, TurnPart::Prose(0)],
        ],
        "one gallery per run of pictures; one picture alone stays a preview"
    );
    // The viewer browses the gallery that was clicked, not every picture in
    // the conversation.
    let run = |id: &str| -> Vec<String> {
        runtime
            .image_run(id)
            .iter()
            .map(|item| item.id.clone())
            .collect()
    };
    assert_eq!(
        run("read-fast:image:0"),
        [
            "read-slow:image:0",
            "read-fast:image:0",
            "read-other:image:0"
        ]
    );
    assert_eq!(run("pasted-2"), ["pasted-1", "pasted-2"]);
    assert_eq!(run("read-narrow:image:0"), ["read-narrow:image:0"]);
    // Each picture, and so each Read's own scroll target, is reachable
    // through the gallery's row.
    let turn = &runtime.turns[0];
    let gallery = TurnPart::Gallery {
        prompt: false,
        start: 0,
        len: 3,
    };
    assert_eq!(
        runtime
            .turn_gallery(turn, gallery)
            .iter()
            .map(|item| images::tiles::label(&item.content, 0))
            .collect::<Vec<_>>(),
        [
            "proposed-slow.png",
            "proposed-fast.png",
            "proposed-other.png"
        ]
    );
}

#[derive(Default)]
struct Tiles {
    controls: Vec<Rectangle>,
    texts: Vec<String>,
}
impl Operation for Tiles {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn focusable(&mut self, _: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
        self.controls.push(bounds);
    }
    fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
        self.texts.push(text.to_owned());
    }
}

struct Drawn {
    size: Size,
    tiles: Vec<Rectangle>,
    names: Vec<String>,
    texts: Vec<String>,
    /// Where the transcript places the row before drawing it.
    estimate: f32,
}

/// The row a run of `count` pictures an agent read draws, laid out `width`
/// wide.
fn drawn(count: usize, width: f32) -> Drawn {
    let mut items = vec![said("ask", chat::Role::User, "Check the breakpoints.")];
    for index in 0..count {
        items.push(read(&format!("read-{index}"), &format!("bp-{index}.png")));
    }
    for index in 0..count {
        items.push(picture(
            &format!("read-{index}:image:0"),
            chat::Role::Assistant,
            &format!("bp-{index}.png"),
        ));
    }
    drawn_row(&items, width)
}

/// The row of `items`' first turn that is not its prompt or steps.
fn drawn_row(items: &[chat::Item], width: f32) -> Drawn {
    let (mut runtime, _handle) = frame_cost::live_runtime(items);
    // Wide, tall and square pictures alike fill the same tiles.
    decoded(&mut runtime, &[(1568, 1045), (720, 1440), (1200, 1200)]);
    let turn = &runtime.turns[0];
    let part = parts(&runtime)[0]
        .iter()
        .copied()
        .find(|part| !matches!(part, TurnPart::Prompt(_) | TurnPart::Steps))
        .unwrap();
    let estimate = runtime.gallery_estimate(turn, part, width);
    let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
    let mut element = runtime.turn_part_view(turn, part, settings);
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(width, 10_000.0)),
    );
    let mut tiles = Tiles::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut tiles);
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    let names = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect();
    Drawn {
        size: node.size(),
        tiles: tiles.controls,
        names,
        texts: tiles.texts,
        estimate,
    }
}

#[test]
fn a_gallery_wraps_uniform_tiles_to_its_width_and_one_picture_stays_a_capped_preview() {
    let inset = 2.0 * f32::from(images::tiles::INSET);
    for width in [720.0, 360.0] {
        let alone = drawn(1, width);
        assert_eq!(alone.tiles.len(), 1);
        assert!(
            alone.size.height <= images::preview::MAX_HEIGHT + inset + 0.5
                && alone.size.width <= width,
            "one picture is a bounded preview at {width}: {:?}",
            alone.size
        );
        assert!(
            !alone
                .texts
                .iter()
                .any(|text| text.contains("Click to enlarge")),
            "{:?}",
            alone.texts
        );
        for count in [2, 3, 7] {
            let gallery = drawn(count, width);
            assert_eq!(gallery.tiles.len(), count, "{count} at {width}");
            let tile = gallery.tiles[0];
            assert!(
                gallery
                    .tiles
                    .iter()
                    .all(|other| (other.height - tile.height).abs() < 0.5
                        && (other.width - tile.width).abs() < 0.5),
                "tiles share one size: {:?}",
                gallery.tiles
            );
            assert!(
                tile.height <= images::tiles::TILE_HEIGHT + inset + 0.5
                    && tile.width <= images::tiles::TILE_WIDTH + 0.5,
                "{tile:?}"
            );
            assert!(gallery.size.width <= width + 0.5, "{:?}", gallery.size);
            let mut rows: Vec<f32> = gallery.tiles.iter().map(|tile| tile.y).collect();
            rows.dedup_by(|a, b| (*a - *b).abs() < 0.5);
            let across = count.div_ceil(rows.len());
            let expected = match (count, width) {
                // Side by side in a wide column; wrapping onto rows of two
                // at a phone's width.
                (2, _) => 2,
                (3, 720.0) => 3,
                (7, 720.0) => 4,
                (_, _) => 2,
            };
            assert_eq!(across, expected, "{count} at {width}: {:?}", gallery.tiles);
            // Each tile is captioned with its file, named for assistive
            // technology, and opens on its own.
            assert_eq!(
                gallery.names,
                (0..count)
                    .map(|index| format!("bp-{index}.png"))
                    .collect::<Vec<_>>()
            );
            assert!(gallery.texts.contains(&format!("bp-{}.png", count - 1)));
            assert!(
                (gallery.estimate - gallery.size.height).abs() < 24.0,
                "the estimate places the gallery near where it lands: {} vs {:?}",
                gallery.estimate,
                gallery.size
            );
        }
    }
}

#[test]
fn pictures_pasted_together_share_a_gallery_within_their_bubble() {
    for width in [720.0, 360.0] {
        let mut items = vec![said("mine", chat::Role::User, "Here are mine:")];
        for index in 1..=4 {
            items.push(picture(
                &format!("pasted-{index}"),
                chat::Role::User,
                &format!("Image {index}"),
            ));
        }
        items.push(said("then", chat::Role::Assistant, "Thanks."));
        let gallery = drawn_row(&items, width);
        assert_eq!(gallery.tiles.len(), 4, "at {width}");
        // A pasted picture's name is its place, not a file, so no caption.
        assert!(
            !gallery.texts.iter().any(|text| text.starts_with("Image ")),
            "{:?}",
            gallery.texts
        );
        assert_eq!(gallery.names, ["Image 1", "Image 2", "Image 3", "Image 4"]);
        let bubble = width.min(PROMPT_MEASURE);
        assert!(
            gallery
                .tiles
                .iter()
                .all(|tile| tile.x >= width - bubble - 0.5 && tile.x + tile.width <= width + 0.5),
            "the tiles are within the bubble at {width}: {:?}",
            gallery.tiles
        );
        assert!(
            (gallery.estimate - gallery.size.height).abs() < 24.0,
            "at {width}: {} vs {:?}",
            gallery.estimate,
            gallery.size
        );
    }
}

#[test]
fn loading_one_picture_of_an_older_gallery_loads_the_gallery() {
    use chat::Role::{Assistant, User};
    let mut items = vec![said("ask", User, "Compare the layouts.")];
    for name in ["old-a.png", "old-b.png", "old-c.png"] {
        items.push(read(&format!("read-{name}"), name));
    }
    for name in ["old-a.png", "old-b.png", "old-c.png"] {
        items.push(picture(&format!("read-{name}:image:0"), Assistant, name));
    }
    // Newer pictures than the transcript keeps loaded on its own.
    for index in 0..images::RECENT_IMAGES {
        items.push(said(&format!("again-{index}"), User, "And this one?"));
        items.push(picture(&format!("new-{index}"), User, "Image 1"));
    }
    let (mut runtime, _handle) = frame_cost::live_runtime(&items);
    runtime
        .visible
        .store(true, std::sync::atomic::Ordering::Relaxed);
    images::sync(&mut runtime);
    let reading = |runtime: &Runtime, id: &str| runtime.cache[id].image_source.is_some();
    assert!(!reading(&runtime, "read-old-b.png:image:0"));
    images::request(&mut runtime, "read-old-a.png:image:0");
    for id in [
        "read-old-a.png:image:0",
        "read-old-b.png:image:0",
        "read-old-c.png:image:0",
    ] {
        assert!(reading(&runtime, id), "{id} is read with its gallery");
    }
}
