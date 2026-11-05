use super::*;
use crate::agent::Provider;
use cosmic::iced::{
    advanced::{Layout, Shell, clipboard, layout, widget::Tree},
    keyboard,
};

const WIDTH: f32 = 900.0;
const HEIGHT: f32 = 720.0;

/// Real draft runtimes and shared-view projection, with all asynchronous Tasks
/// dropped: no provider, daemon, or persistent application state is involved.
fn full_width_draft(app: &mut Cagents, provider: Provider) -> Pane {
    app.settings.default_native_pane_fraction = 1.0;
    drop(app.new_native_chat(provider, Some(PathBuf::from("/repo"))));
    let card = app.workspace.focused().unwrap();
    app.card_state_mut(card).unwrap().width_fraction = 1.0;
    let update = app
        .workspace
        .set_card_width(card, CardWidth::Fraction(1.0))
        .unwrap();
    drop(app.apply_viewport(update, false, false));
    Pane::from_card(app.card_state(card).unwrap(), None).unwrap()
}

fn peer_insertion_fixture() -> (Cagents, View, CardId, CardId) {
    let (mut app, channel, _, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.workspace.resize_viewport(WIDTH);
    let local = full_width_draft(&mut app, Provider::Codex);
    app.shared_views
        .baseline
        .insert(Scope::Channel(channel), vec![local.clone()]);
    app.shared_views.pending.clear();

    let mut peer = super::super::super::tests::context_inspector_test_app(false);
    peer.active_workspace = Some(PathBuf::from("/repo"));
    peer.workspace.resize_viewport(WIDTH);
    let incoming = full_width_draft(&mut peer, Provider::Claude);
    let view = View {
        revision: 2,
        panes: vec![incoming.clone(), local.clone()],
        ..View::empty(channel)
    };
    (app, view, local.id, incoming.id)
}

fn adopt_peer_insertion(app: &mut Cagents, view: View) {
    app.workspace_model.views = vec![view];
    app.shared_views.pending.clear();
    drop(app.reconcile_shared_views());
}

/// A passive insertion before the pane being read must preserve that pane's
/// place in the viewport as well as its selection. Preserving only the card ID
/// highlighted Codex 2/2 while the camera at zero painted the incoming Claude.
#[test]
fn a_peer_inserting_before_the_focused_pane_keeps_its_body_in_view() {
    let (mut app, view, local, incoming) = peer_insertion_fixture();
    assert_eq!(app.workspace.camera().offset(), 0.0);
    adopt_peer_insertion(&mut app, view);

    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [incoming, local]);
    assert_eq!(app.workspace.focused(), Some(local));
    assert_eq!(app.workspace.focused_index(), Some(1));
    let projection = app.workspace.projection(WIDTH);
    let body = projection
        .mounted
        .iter()
        .find(|card| card.id == local)
        .expect("the selected conversation remains mounted");
    assert_eq!(body.visibility, VisibilityClass::Visible);
    assert!(
        body.start < projection.offset + WIDTH && body.end() > projection.offset,
        "the selected conversation must actually intersect the viewport, not just overscan or an animation destination: {projection:?}"
    );
}

fn workspace_element(app: &Cagents) -> Element<'_, Message> {
    let generation = app.workspace_view_generation;
    workspace_widget::view(
        &app.workspace,
        &app.settings.keyboard_shortcuts,
        app.focused_pane(),
        false,
        &app.input_trace,
        WIDTH,
        app.workspace_scroll_id.clone(),
        |context| app.workspace_card_view(context.id, context.focused, false),
        workspace_widget::card_widget_id,
        workspace_widget::Events {
            viewport: move |event| Message::Viewport { generation, event },
        },
    )
}

/// Retain the actual workspace widget tree across projection. Only the first
/// frame explicitly focuses a composer; the second must retain that editor's
/// focus and route an actual character event through its captured Target.
fn type_through_retained_workspace(
    app: &Cagents,
    tree: &mut Tree,
    renderer: &cosmic::Renderer,
    focus: Option<CardId>,
) -> Vec<Message> {
    let viewport = iced::Rectangle::with_size(iced::Size::new(WIDTH, HEIGHT));
    let limits = layout::Limits::new(viewport.size(), viewport.size());
    let mut element = workspace_element(app);
    tree.diff(element.as_widget_mut());
    let node = element.as_widget_mut().layout(tree, renderer, &limits);
    // Apply the scroll synchronization the application schedules, without
    // executing unrelated asynchronous Tasks in this headless widget test.
    element.as_widget_mut().operate(
        tree,
        Layout::new(&node),
        renderer,
        &mut iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            app.workspace_scroll_id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: Some(app.workspace.camera().offset()),
                y: None,
            },
        ),
    );
    let node = element.as_widget_mut().layout(tree, renderer, &limits);
    if let Some(card) = focus {
        element.as_widget_mut().operate(
            tree,
            Layout::new(&node),
            renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                card,
                "chat-composer",
            )),
        );
    }
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        tree,
        &iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("x".into()),
            modified_key: keyboard::Key::Character("x".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("x".into()),
            repeat: false,
        }),
        Layout::new(&node),
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn a_peer_insertion_keeps_typing_in_the_retained_visible_composer() {
    let renderer = super::super::super::tests::headless_renderer();
    let (mut app, view, local, incoming) = peer_insertion_fixture();
    let target = match app.cards.get(&local).unwrap() {
        CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("the local pane is a draft chat"),
    };
    let mut tree = Tree::empty();
    let first = type_through_retained_workspace(&app, &mut tree, &renderer, Some(local));
    assert!(first.iter().any(|message| matches!(
        message,
        Message::ChatUi(chat_ui::Action::Composer(actual, action))
            if *actual == target && action.is_edit()
    )));
    for message in first {
        drop(app.update(message));
    }
    adopt_peer_insertion(&mut app, view);

    let second = type_through_retained_workspace(&app, &mut tree, &renderer, None);
    let edited: Vec<_> = second
        .iter()
        .filter_map(|message| match message {
            Message::ChatUi(chat_ui::Action::Composer(actual, action)) if action.is_edit() => {
                Some(*actual)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        edited,
        [target],
        "the retained editor keeps its exact target"
    );
    for message in second {
        drop(app.update(message));
    }
    let saved = app.persistent_workspace();
    assert_eq!(
        saved.chat_drafts.get(&local).map(String::as_str),
        Some("xx")
    );
    assert!(!saved.chat_drafts.contains_key(&incoming));
}
