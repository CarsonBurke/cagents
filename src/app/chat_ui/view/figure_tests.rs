//! Markdown images in the actual transcript view.

use super::*;
use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
use iced::{Event, Rectangle, Size, mouse};

fn answer(text: String) -> chat::Item {
    chat::Item {
        id: "answer".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text,
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    }
}

/// An answer long enough that its blocks are laid out one by one, with an
/// image among them.
fn document() -> chat::Item {
    let mut text = String::new();
    for index in 0..6 {
        text.push_str(&format!("Paragraph {index} before the mockup.\n\n"));
    }
    text.push_str("![Mockup](/var/tmp/mockup.png)\n\nAnd one after it.\n");
    answer(text)
}

fn view(runtime: &Runtime) -> Element<'_, Message> {
    runtime.view(
        None,
        false,
        true,
        &Default::default(),
        620.0,
        Place {
            name: "figures".into(),
            branch: None,
            channel: None,
        },
        true,
        None,
        vec![],
    )
}

/// What the transcript asks for when it is drawn once.
fn redraw(runtime: &Runtime) -> Vec<Message> {
    let renderer = crate::app::tests::headless_renderer();
    let mut element = view(runtime);
    let mut tree = Tree::new(&element);
    let size = Size::new(620.0, 720.0);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, size),
    );
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Window(iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &Rectangle::with_size(size),
    );
    messages
}

#[test]
fn an_answer_on_screen_asks_for_its_local_image() {
    let (runtime, _handle) = prepared_runtime_for_test(&[document()]);
    let names: Vec<_> = redraw(&runtime)
        .iter()
        .filter_map(|message| match message {
            Message::ChatUi(action) => Some(action.name()),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&"LoadFigure"),
        "the transcript asked for {names:?}"
    );
}

/// The text the transcript shows, read from a tree kept across frames.
fn texts(runtime: &Runtime, tree: &mut Option<Tree>) -> Vec<String> {
    use iced::advanced::widget::Operation;
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = view(runtime);
    let tree = match tree {
        Some(tree) => {
            tree.diff(element.as_widget_mut());
            tree
        }
        None => tree.insert(Tree::new(&element)),
    };
    let size = Size::new(620.0, 720.0);
    let node =
        element
            .as_widget_mut()
            .layout(tree, &renderer, &layout::Limits::new(Size::ZERO, size));
    let mut texts = Texts(Vec::new());
    element
        .as_widget_mut()
        .operate(tree, Layout::new(&node), &renderer, &mut texts);
    texts.0
}

#[test]
fn a_decoded_preview_replaces_its_loading_line_in_the_kept_exchange() {
    let (mut runtime, _handle) = prepared_runtime_for_test(&[document()]);
    let mut tree = None;
    let before = texts(&runtime, &mut tree);
    assert!(
        before.iter().any(|text| text == "Loading image…"),
        "{before:?}"
    );
    let id = redraw(&runtime)
        .into_iter()
        .find_map(|message| match message {
            Message::ChatUi(Action::LoadFigure(_, id)) => Some(id),
            _ => None,
        })
        .expect("asked for the image");
    let attempt = runtime.figures.begin_for_test(id);
    drop(images::figure::complete(
        &mut runtime,
        id,
        attempt,
        images::figure::Outcome::ready_for_test(),
    ));
    let after = texts(&runtime, &mut tree);
    assert!(
        !after.iter().any(|text| text == "Loading image…"),
        "{after:?}"
    );
}
