use super::*;
use cosmic::iced::advanced::{Layout, Shell, clipboard as native_clipboard, layout, widget::Tree};
use cosmic::iced::{Event, Rectangle, Size, mouse};

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut bytes),
        &vec![90; (width * height * 4) as usize],
        width,
        height,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

fn origin(root: &Path) -> Origin {
    Origin {
        root: root.to_owned(),
        child: false,
        device: None,
    }
}

fn handle(width: u32, height: u32) -> iced::widget::image::Handle {
    iced::widget::image::Handle::from_rgba(width, height, vec![0; (width * height * 4) as usize])
}

fn uri(text: &str) -> markdown::Uri {
    Arc::from(text)
}

fn ready(path: &str) -> Outcome {
    Outcome::Ready {
        handle: handle(2, 1),
        path: path.into(),
        stamp: Some(Stamp {
            modified: None,
            len: 1,
        }),
    }
}

#[test]
fn destinations_are_local_files_web_addresses_or_neither() {
    assert_eq!(kind("/var/tmp/mockup/x.png"), Kind::Local);
    assert_eq!(kind("docs/x.png"), Kind::Local);
    assert_eq!(kind("file:///var/tmp/a%20b.png"), Kind::Local);
    assert_eq!(kind("https://example.com/x.png"), Kind::Web);
    assert_eq!(kind("HTTP://example.com/x.png"), Kind::Web);
    assert_eq!(kind("data:image/png;base64,AAAA"), Kind::Other);
    assert_eq!(kind("file://host/x.png"), Kind::Other);
    assert_eq!(kind(""), Kind::Other);
}

#[test]
fn an_image_names_the_file_a_link_in_the_answer_would_open() {
    let root = Path::new("/work");
    for (url, child, file) in [
        ("docs/x.png", false, Some("/work/docs/x.png")),
        ("./docs/../x.png", false, Some("/work/x.png")),
        ("my%20mockup.png", false, Some("/work/my mockup.png")),
        ("x.png?raw=1#top", false, Some("/work/x.png")),
        ("file:///var/tmp/a%20b.png", true, Some("/var/tmp/a b.png")),
        ("/abs/x.png", true, Some("/abs/x.png")),
        ("docs/x.png", true, None),
    ] {
        assert_eq!(
            located(url, root, child),
            file.map(PathBuf::from),
            "{url} in a child conversation: {child}"
        );
        if let Some(file) = file {
            let opened = crate::document_link::resolve(
                if child {
                    Path::new("/.chat")
                } else {
                    Path::new("/work/.chat")
                },
                url,
            );
            assert!(
                matches!(opened, Ok(crate::document_link::DocumentLink::Local { path, .. }) if path == Path::new(file)),
                "Open image source opens the file previewed for {url}"
            );
        }
    }
    assert_eq!(located("docs/x.png", Path::new("relative"), false), None);
}

#[test]
fn a_local_image_is_read_once_then_only_checked_until_its_file_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mockup.png");
    std::fs::write(&path, png(3, 2)).unwrap();
    let Outcome::Ready {
        handle,
        path: shown,
        stamp,
    } = read("mockup.png", origin(directory.path()), None)
    else {
        panic!("a readable image decodes");
    };
    assert_eq!(shown, path, "a relative path is shown joined to the root");
    assert!(matches!(
        handle,
        iced::widget::image::Handle::Rgba {
            width: 3,
            height: 2,
            ..
        }
    ));
    assert!(matches!(
        read(path.to_str().unwrap(), origin(directory.path()), stamp),
        Outcome::Unchanged
    ));
    std::fs::write(&path, png(4, 4)).unwrap();
    assert!(
        matches!(
            read(path.to_str().unwrap(), origin(directory.path()), stamp),
            Outcome::Ready { .. }
        ),
        "a file rewritten under the same name is decoded again"
    );
}

#[test]
fn images_that_cannot_be_shown_say_why() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.png"), b"not an image").unwrap();
    let reason = |path: &str, origin: Origin| match read(path, origin, None) {
        Outcome::Failed { reason, .. } => reason,
        _ => panic!("{path} should not decode"),
    };
    assert_eq!(
        reason("missing.png", self::origin(directory.path())),
        Reason::Missing
    );
    match read("missing.png", self::origin(directory.path()), None) {
        Outcome::Failed { detail, .. } => assert_eq!(
            detail,
            directory.path().join("missing.png").display().to_string(),
            "a missing file's tip names the file it looked for"
        ),
        _ => panic!("missing.png should not decode"),
    }
    assert_eq!(
        reason("notes.png", self::origin(directory.path())),
        Reason::NotImage
    );
    assert_eq!(
        reason(".", self::origin(directory.path())),
        Reason::NotImage,
        "a directory is not an image"
    );
    let child = Origin {
        child: true,
        ..self::origin(directory.path())
    };
    assert_eq!(reason("notes.png", child), Reason::Unavailable);
    let Outcome::Failed { stamp, .. } = read("notes.png", origin(directory.path()), None) else {
        panic!("notes.png should not decode");
    };
    assert!(
        matches!(
            read("notes.png", origin(directory.path()), stamp),
            Outcome::Unchanged
        ),
        "a file that is not an image is read again only once it changes"
    );
}

#[test]
fn a_preview_redraws_only_when_what_it_draws_changes() {
    let figures = Figures::default();
    let id = figures.store().id(Key {
        url: uri("/x.png"),
        child: false,
    });
    let before = figures.version();
    let request = figures.begin(id, false, false).expect("first sight loads");
    assert!(request.known.is_none());
    assert_eq!(figures.version(), before, "loading draws as not loaded");
    assert!(
        figures.begin(id, false, false).is_none(),
        "one load at a time"
    );
    assert!(
        figures.begin(id, true, false).is_none(),
        "a child transcript names its own images"
    );
    // An answer to an older request is stale.
    figures.complete(id, request.attempt.wrapping_sub(1), ready("/x.png"));
    assert_eq!(figures.version(), before);
    figures.complete(id, request.attempt, ready("/x.png"));
    let shown = figures.version();
    assert_ne!(shown, before);
    assert!(
        figures.begin(id, false, false).is_none(),
        "a preview just checked is trusted"
    );
    if let State::Ready { checked, .. } = &mut figures.store().entries.get_mut(&id).unwrap().state {
        *checked -= REVALIDATE;
    }
    let check = figures
        .begin(id, false, false)
        .expect("seen again later, its file is checked");
    assert!(check.known.is_some());
    figures.complete(id, check.attempt, Outcome::Unchanged);
    assert_eq!(
        figures.version(),
        shown,
        "an unchanged file redraws nothing"
    );
}

#[test]
fn another_devices_preview_is_not_rechecked_without_a_stamp() {
    let figures = Figures::default();
    let id = figures.store().id(Key {
        url: uri("/x.png"),
        child: false,
    });
    let request = figures.begin(id, false, true).unwrap();
    figures.complete(
        id,
        request.attempt,
        Outcome::Ready {
            handle: handle(1, 1),
            path: "/x.png".into(),
            stamp: None,
        },
    );
    if let State::Ready { checked, .. } = &mut figures.store().entries.get_mut(&id).unwrap().state {
        *checked -= REVALIDATE;
    }
    assert!(figures.begin(id, false, true).is_none());
}

#[test]
fn previews_beyond_the_limit_give_their_pixels_back_and_keep_their_room() {
    let figures = Figures::default();
    let ids: Vec<u64> = (0..READY + 2)
        .map(|index| {
            let id = figures.store().id(Key {
                url: uri(&format!("/{index}.png")),
                child: false,
            });
            let request = figures.begin(id, false, false).unwrap();
            figures.complete(id, request.attempt, ready("/x.png"));
            id
        })
        .collect();
    let store = figures.store();
    let ready = store
        .entries
        .values()
        .filter(|entry| matches!(entry.state, State::Ready { .. }))
        .count();
    assert_eq!(ready, READY);
    for id in &ids[..2] {
        assert!(
            matches!(
                store.entries[id].state,
                State::Unseen { size: Some((2, 1)) }
            ),
            "the least recently seen are released first"
        );
    }
}

#[test]
fn a_hidden_conversation_releases_its_previews_and_another_directory_forgets_them() {
    let figures = Figures::default();
    figures.sync(Path::new("/work"), None, true);
    let id = figures.store().id(Key {
        url: uri("docs/x.png"),
        child: false,
    });
    let request = figures.begin(id, false, false).unwrap();
    figures.complete(id, request.attempt, ready("/work/docs/x.png"));
    figures.sync(Path::new("/work"), None, false);
    assert!(matches!(
        figures.store().entries[&id].state,
        State::Unseen { size: Some(_) }
    ));
    let request = figures.begin(id, false, false).unwrap();
    figures.complete(id, request.attempt, ready("/work/docs/x.png"));
    figures.sync(Path::new("/elsewhere"), None, true);
    assert!(matches!(
        figures.store().entries[&id].state,
        State::Unseen { size: None }
    ));
}

#[test]
fn memory_pressure_waits_without_hiding_a_shown_preview() {
    let figures = Figures::default();
    let id = figures.store().id(Key {
        url: uri("/x.png"),
        child: false,
    });
    let request = figures.begin(id, false, false).unwrap();
    let Loaded::Deferred(pressure) = super::super::deferred_for_test() else {
        unreachable!()
    };
    assert!(matches!(
        figures.complete(id, request.attempt, Outcome::Deferred(pressure.clone())),
        Next::Wait(_)
    ));
    assert!(matches!(figures.store().entries[&id].state, State::Waiting));
    let request = figures
        .begin(id, false, false)
        .expect("memory freed, it loads again");
    figures.complete(id, request.attempt, ready("/x.png"));
    if let State::Ready { checked, .. } = &mut figures.store().entries.get_mut(&id).unwrap().state {
        *checked -= REVALIDATE;
    }
    let check = figures.begin(id, false, false).unwrap();
    assert!(matches!(
        figures.complete(id, check.attempt, Outcome::Deferred(pressure)),
        Next::Nothing
    ));
    assert!(matches!(
        figures.store().entries[&id].state,
        State::Ready { .. }
    ));
}

#[test]
fn copy_finds_the_preview_drawn_with_the_pixels_clicked() {
    let figures = Figures::default();
    let id = figures.store().id(Key {
        url: uri("/x.png"),
        child: false,
    });
    let request = figures.begin(id, false, false).unwrap();
    let pixels = handle(1, 1);
    figures.complete(
        id,
        request.attempt,
        Outcome::Ready {
            handle: pixels.clone(),
            path: "/x.png".into(),
            stamp: None,
        },
    );
    let resolved = figures.resolve(pixels.id()).expect("the shown preview");
    assert_eq!(resolved.path.as_deref(), Some("/x.png"));
    assert!(figures.resolve(handle(1, 1).id()).is_none());
}

/// The element `shared` draws for `url`, laid out in a 600 by 800 viewport.
fn laid_out(
    shared: &Shared,
    target: Target,
    url: &str,
) -> (Element<'static, Message>, Tree, layout::Node) {
    let mut element = shared.view(target, &uri(url), "Mockup");
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(600.0, 800.0)),
    );
    (element, tree, node)
}

fn redraw(
    element: &mut Element<'static, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    viewport: Rectangle,
) -> Vec<Message> {
    let renderer = crate::app::tests::headless_renderer();
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        tree,
        &Event::Window(iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )),
        Layout::new(node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut native_clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn a_local_image_asks_to_load_only_near_the_viewport_and_a_web_image_never() {
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let shared = Figures::default().shared(false);
    let (mut element, mut tree, node) = laid_out(&shared, target, "docs/mockup.png");
    let far = Rectangle::new(iced::Point::new(0.0, 5_000.0), Size::new(600.0, 800.0));
    assert!(redraw(&mut element, &mut tree, &node, far).is_empty());
    let near = Rectangle::new(iced::Point::ORIGIN, Size::new(600.0, 800.0));
    let messages = redraw(&mut element, &mut tree, &node, near);
    assert!(
        matches!(
            messages.as_slice(),
            [Message::ChatUi(Action::LoadFigure(found, _))] if *found == target
        ),
        "a local image asks to load once it is near"
    );
    let (mut element, mut tree, node) = laid_out(&shared, target, "https://example.com/x.png");
    assert!(
        redraw(&mut element, &mut tree, &node, near).is_empty(),
        "a web image is never fetched"
    );
}

#[test]
fn a_preview_keeps_to_the_reading_measure_and_its_height_cap() {
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let figures = Figures::default();
    let shared = figures.shared(false);
    for (width, height, url) in [(1600, 900, "/wide.png"), (400, 2000, "/tall.png")] {
        let id = figures.store().id(Key {
            url: uri(url),
            child: false,
        });
        let request = figures.begin(id, false, false).unwrap();
        figures.complete(
            id,
            request.attempt,
            Outcome::Ready {
                handle: handle(width, height),
                path: url.into(),
                stamp: None,
            },
        );
        let (_, _, node) = laid_out(&shared, target, url);
        let preview = node.children()[0].bounds();
        assert!(preview.width <= 600.0, "{url} fits the measure");
        assert!(
            preview.height <= super::super::preview::MAX_HEIGHT + 0.5,
            "{url} keeps to the cap"
        );
        let ratio = preview.width / preview.height;
        assert!(
            (ratio - width as f32 / height as f32).abs() < 0.05,
            "{url} keeps its aspect ratio: {preview:?}"
        );
    }
}

fn asks(messages: &[Message], target: Target) -> bool {
    matches!(
        messages,
        [Message::ChatUi(Action::LoadFigure(found, _))] if *found == target
    )
}

/// `shared`'s element for `url` drawn again, as a kept row's next view is,
/// over the widget state the last one left.
fn drawn_again(
    shared: &Shared,
    target: Target,
    url: &str,
    tree: &mut Tree,
) -> (Element<'static, Message>, layout::Node) {
    let mut element = shared.view(target, &uri(url), "Mockup");
    tree.diff(element.as_widget_mut());
    let renderer = crate::app::tests::headless_renderer();
    let node = element.as_widget_mut().layout(
        tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(600.0, 800.0)),
    );
    (element, node)
}

#[test]
fn an_image_coming_back_into_view_asks_again() {
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let shared = Figures::default().shared(false);
    let (mut element, mut tree, node) = laid_out(&shared, target, "/x.png");
    let near = Rectangle::new(iced::Point::ORIGIN, Size::new(600.0, 800.0));
    let far = Rectangle::new(iced::Point::new(0.0, 5_000.0), Size::new(600.0, 800.0));
    assert!(asks(&redraw(&mut element, &mut tree, &node, near), target));
    assert!(redraw(&mut element, &mut tree, &node, near).is_empty());
    assert!(matches!(
        redraw(&mut element, &mut tree, &node, far).as_slice(),
        [Message::ChatUi(Action::FigureOutOfSight)]
    ));
    assert!(
        asks(&redraw(&mut element, &mut tree, &node, near), target),
        "scrolled back, a preview checks its file again"
    );
}

#[test]
fn an_image_that_could_not_be_shown_is_looked_at_again_while_in_view() {
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let figures = Figures::default();
    let shared = figures.shared(false);
    let (mut element, mut tree, node) = laid_out(&shared, target, "/x.png");
    let near = Rectangle::new(iced::Point::ORIGIN, Size::new(600.0, 800.0));
    assert!(asks(&redraw(&mut element, &mut tree, &node, near), target));
    let id = figures.store().ids[&Key {
        url: uri("/x.png"),
        child: false,
    }];
    let request = figures.begin(id, false, false).unwrap();
    let failed = Outcome::Failed {
        reason: Reason::Missing,
        detail: "/x.png".into(),
        stamp: None,
    };
    assert!(matches!(
        figures.complete(id, request.attempt, failed),
        Next::Recheck
    ));
    let (mut element, node) = drawn_again(&shared, target, "/x.png", &mut tree);
    assert!(
        redraw(&mut element, &mut tree, &node, near).is_empty(),
        "the failure is shown without asking again at once"
    );
    let before = figures.version();
    figures.recheck(id);
    assert_ne!(figures.version(), before, "the line draws again");
    let (mut element, node) = drawn_again(&shared, target, "/x.png", &mut tree);
    assert!(
        asks(&redraw(&mut element, &mut tree, &node, near), target),
        "the line still in view asks again"
    );
    if let State::Failed { at, .. } = &mut figures.store().entries.get_mut(&id).unwrap().state {
        *at -= RETRY;
    }
    assert!(
        figures.begin(id, false, false).is_some(),
        "and the file is looked for again"
    );
}

#[test]
fn hiding_a_conversation_drops_loads_in_flight_and_asks_again_once_shown() {
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let figures = Figures::default();
    figures.sync(Path::new("/work"), None, true);
    let shared = figures.shared(false);
    let (mut element, mut tree, node) = laid_out(&shared, target, "/x.png");
    let near = Rectangle::new(iced::Point::ORIGIN, Size::new(600.0, 800.0));
    assert!(asks(&redraw(&mut element, &mut tree, &node, near), target));
    let id = figures.store().ids[&Key {
        url: uri("/x.png"),
        child: false,
    }];
    let request = figures.begin(id, false, false).unwrap();
    figures.sync(Path::new("/work"), None, false);
    assert!(matches!(
        figures.store().entries[&id].state,
        State::Unseen { .. }
    ));
    assert!(
        matches!(
            figures.complete(id, request.attempt, ready("/x.png")),
            Next::Nothing
        ),
        "an answer for the hidden conversation is stale"
    );
    figures.sync(Path::new("/work"), None, true);
    let (mut element, node) = drawn_again(&shared, target, "/x.png", &mut tree);
    assert!(
        asks(&redraw(&mut element, &mut tree, &node, near), target),
        "shown again, the image still in view asks once more"
    );
}

#[test]
fn long_names_and_addresses_are_cut_with_an_ellipsis() {
    assert_eq!(clipped("mockup", 6), "mockup");
    assert_eq!(clipped("mockups", 6), "mocku…");
    assert_eq!(clipped(&"é".repeat(500), TIP).chars().count(), TIP);
}
