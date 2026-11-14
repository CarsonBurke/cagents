use super::*;
use iced::advanced::{
    Layout, Shell, clipboard, layout, mouse,
    widget::{Operation, Tree, operation::Focusable},
};
use iced::{Event, Rectangle, Size};

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
/// click its actual image button using its rendered focusable bounds. The
/// pinned Iced hover wrapper forwards operations/input, but not accessibility.
fn click_prompt(
    renderer: &cosmic::Renderer,
    runtime: &Runtime,
    item: &chat::Item,
) -> (Vec<String>, Vec<Message>) {
    #[derive(Default)]
    struct Prompt {
        texts: Vec<String>,
        controls: Vec<Rectangle>,
    }
    impl Operation for Prompt {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: Rectangle, text: &str) {
            self.texts.push(text.to_owned());
        }
        fn custom(&mut self, _: Option<&widget::Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(prose) = state.downcast_ref::<selectable_prose::State>() {
                self.texts.push(prose.plain.to_string());
            }
        }
        fn focusable(&mut self, _: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
            self.controls.push(bounds);
        }
    }
    let item = runtime
        .active_item(&item.id)
        .expect("prompt fixture renders the actual active transcript source");
    let mut element = runtime.prompt_view(item);
    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::with_size(Size::new(600.0, 600.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut prompt = Prompt::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), renderer, &mut prompt);
    // The preview occupies its rendered image area (280 logical pixels tall).
    // The message-copy footer is a compact icon button outside that area.
    let previews: Vec<_> = prompt
        .controls
        .iter()
        .filter(|bounds| bounds.height >= 200.0)
        .copied()
        .collect();
    let mut messages = Vec::new();
    for bounds in &previews {
        let point = bounds.center();
        for event in [
            mouse::Event::CursorMoved { position: point },
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
    let actions: Vec<_> = messages
        .iter()
        .map(|message| match message {
            Message::ChatUi(Action::OpenImage(target, id)) => {
                format!("OpenImage({target:?}, {id})")
            }
            Message::ChatUi(Action::LoadImage(target, id)) => {
                format!("LoadImage({target:?}, {id})")
            }
            message => format!("{message:?}"),
        })
        .collect();
    eprintln!(
        "rendered prompt {:?}: controls={:?}, preview bounds={previews:?}, actions={actions:?}",
        item.id, prompt.controls
    );
    (prompt.texts, messages)
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
    cached.image_error = Some("The attachment does not provide an image file path.".into());
    let renderer = crate::app::tests::headless_renderer();
    let (texts, messages) = click_prompt(&renderer, &runtime, &item);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("owned image receipt could not be verified"))
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::ChatUi(Action::OpenImage(..) | Action::LoadImage(..))
    )));
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("does not provide an image file path"))
    );
    assert!(!texts.iter().any(|text| text == "Load preview"));
}

#[test]
fn claude_document_placeholder_keeps_its_title_without_image_actions() {
    // Claude's document decoder represents non-pixel attachments with this
    // exact shape. A generic failed image read must not replace their title.
    let item = chat::Item {
        id: "recorded-claude-document".into(),
        role: chat::Role::User,
        content: ItemContent::Image {
            path: String::new(),
            description: "Claude document: release-notes.pdf".into(),
            reference: None,
        },
        started_at: None,
        at: None,
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(std::slice::from_ref(&item));
    let cached = runtime.cache.get_mut(&item.id).unwrap();
    cached.image = None;
    cached.image_source = None;
    cached.image_unavailable = true;
    cached.image_error = Some("The attachment does not provide an image file path.".into());
    let renderer = crate::app::tests::headless_renderer();
    let (texts, messages) = click_prompt(&renderer, &runtime, &item);
    assert!(
        texts
            .iter()
            .any(|text| text == "Claude document: release-notes.pdf")
    );
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("does not provide an image file path"))
    );
    assert!(!texts.iter().any(|text| text == "Load preview"));
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::ChatUi(Action::OpenImage(..) | Action::LoadImage(..))
    )));
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
