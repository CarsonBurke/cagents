use super::*;
use iced::advanced::{
    Layout, Shell, clipboard, layout, mouse,
    widget::{Operation, Tree},
};
use iced::{Event, Point, Rectangle, Size};

fn image(id: &str, number: u64) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: chat::Role::User,
        content: ItemContent::Image {
            path: "/private/transcript/image-internal-digest.png".into(),
            description: format!("Image {number}"),
            reference: Some(chat::ImageReference {
                id: uuid::Uuid::new_v4(),
                number,
                device: uuid::Uuid::new_v4(),
                name: "Pasted image".into(),
                sha256: "a".repeat(64),
                extension: "png".into(),
            }),
        },
        started_at: None,
        at: None,
    }
}

fn ready(cached: &mut CachedItem) {
    cached.image = Some(iced::widget::image::Handle::from_rgba(
        1,
        1,
        vec![12, 34, 56, 255],
    ));
    cached.image_source = None;
    cached.image_unavailable = false;
    cached.image_error = None;
}

/// Lay out the actual prompt widget, inspect its displayed text and
/// click its real non-copy controls using their accessibility bounds.
fn click_prompt(
    renderer: &cosmic::Renderer,
    runtime: &Runtime,
    item: &chat::Item,
) -> (Vec<String>, Vec<Message>) {
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
        fn custom(&mut self, _: Option<&widget::Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push(prose.plain.to_string());
            }
        }
    }
    let mut element = runtime.prompt_view(item);
    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::with_size(Size::new(600.0, 600.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut texts = Texts::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), renderer, &mut texts);
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let controls: Vec<_> = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() != Some("Copy this message")
        })
        .filter_map(|node| node.node().bounds())
        .map(|bounds| {
            Point::new(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            )
        })
        .collect();
    let mut messages = Vec::new();
    for point in controls {
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
    }
    (texts.0, messages)
}

#[test]
fn restored_user_image_opens_viewer_from_its_rendered_preview() {
    let item = image("restored-user-image", 17);
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    ready(runtime.cache.get_mut(&item.id).unwrap());
    let renderer = crate::app::tests::headless_renderer();
    let (texts, messages) = click_prompt(&renderer, &runtime, &item);
    assert!(
        texts
            .iter()
            .any(|text| text == "Image 17 · Click to enlarge")
    );
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("image-internal-digest"))
    );
    assert!(matches!(messages.as_slice(),
        [Message::ChatUi(Action::OpenImage(target, id))]
            if *target == runtime.target && id == &item.id));
    let Message::ChatUi(open) = messages.into_iter().next().unwrap() else {
        unreachable!()
    };
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.handle_chat_ui(open));
    let gallery = app
        .chat_ui
        .image_viewer
        .as_ref()
        .expect("clicked user image opens viewer");
    assert!(
        gallery.url().is_none(),
        "a managed hint is never an external-open target"
    );
}

#[test]
fn unavailable_user_image_preserves_its_error_without_a_viewer_action() {
    let mut item = image("wrong-device-image", 23);
    let ItemContent::Image {
        path,
        description,
        reference,
    } = &mut item.content
    else {
        unreachable!()
    };
    path.clear();
    *description =
        "Image 23 is unavailable on this device: its owned image receipt could not be verified."
            .into();
    *reference = None;
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let cached = runtime.cache.get_mut(&item.id).unwrap();
    cached.image = None;
    cached.image_source = None;
    cached.image_unavailable = true;
    let renderer = crate::app::tests::headless_renderer();
    let (texts, messages) = click_prompt(&renderer, &runtime, &item);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("owned image receipt could not be verified"))
    );
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::ChatUi(Action::OpenImage(..))))
    );
}

#[test]
fn selected_child_user_image_uses_its_own_preview_despite_parent_id_collision() {
    let parent = image("same-image-id", 31);
    let child = image(&parent.id, 47);
    let (mut runtime, handle) = frame_cost::live_runtime(std::slice::from_ref(&parent));
    let cached = runtime.cache.get_mut(&parent.id).unwrap();
    cached.image = None;
    cached.image_source = None;
    cached.image_unavailable = true;
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents.push(chat::Subagent {
        id: "native-child".into(),
        kind: "Scout".into(),
        description: "Image fixture".into(),
        status: chat::ToolStatus::Succeeded,
        model: None,
        waiting: false,
        detail: String::new(),
        background: false,
        items: vec![Arc::new(child.clone())],
    });
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("native-child".into()))));
    let renderer = crate::app::tests::headless_renderer();
    let runtime = app.chat_runtime_mut(target).unwrap();
    ready(runtime.child_cache.get_mut(&child.id).unwrap());
    let (texts, messages) = click_prompt(&renderer, runtime, &child);
    assert!(
        texts
            .iter()
            .any(|text| text == "Image 47 · Click to enlarge")
    );
    assert!(matches!(messages.as_slice(),
        [Message::ChatUi(Action::OpenImage(found, id))] if *found == target && id == &child.id));
    let Message::ChatUi(open) = messages.into_iter().next().unwrap() else {
        unreachable!()
    };
    drop(app.handle_chat_ui(open));
    assert!(app.chat_ui.image_viewer.is_some());
    drop(app.handle_chat_ui(Action::CloseImage));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    let (_, messages) = click_prompt(&renderer, runtime, &parent);
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::ChatUi(Action::OpenImage(..)))),
        "child pixels never make the parent clickable"
    );
}

#[test]
fn uncached_managed_user_image_keeps_its_stable_label() {
    let item = image("uncached-user-image", 59);
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    runtime.cache.remove(&item.id);
    let renderer = crate::app::tests::headless_renderer();
    let (texts, messages) = click_prompt(&renderer, &runtime, &item);
    assert!(texts.iter().any(|text| text == "Image 59"));
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("image-internal-digest"))
    );
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::ChatUi(Action::OpenImage(..))))
    );
}
