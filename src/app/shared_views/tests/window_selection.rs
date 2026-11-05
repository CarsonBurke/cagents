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

fn four_shared_drafts(
    select_reading: bool,
    fraction: f32,
) -> (
    Cagents,
    ChannelId,
    ChannelId,
    [Pane; 4],
    [chat_ui::Target; 4],
) {
    let (mut app, source, destination, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(source);
    app.workspace.resize_viewport(WIDTH);
    let panes = std::array::from_fn(|index| {
        full_width_draft(
            &mut app,
            if index % 2 == 0 {
                Provider::Codex
            } else {
                Provider::Claude
            },
        )
    });
    app.workspace_model.views = vec![
        View {
            revision: 1,
            panes: panes.to_vec(),
            ..View::empty(source)
        },
        View::empty(destination),
    ];
    app.shared_views.pending.clear();
    drop(app.reconcile_shared_views());
    let selected = panes[if select_reading { 2 } else { 0 }].id;
    let update = app.workspace.focus(selected).unwrap();
    drop(app.apply_viewport(update, false, false));
    let update = app.workspace.tick(Duration::from_secs(1));
    drop(app.apply_viewport(update, false, false));
    // A deliberate pan can leave selection elsewhere. Both the selected and
    // unselected reading cases must retain this local viewport intent.
    let update = app.workspace.native_scroll(WIDTH * (2.0 + fraction), WIDTH);
    drop(app.apply_viewport(update, false, false));
    assert_eq!(app.workspace.focused(), Some(selected));
    assert_reading_position(&app, panes[2].id, fraction);
    let targets = std::array::from_fn(|index| match app.cards.get(&panes[index].id).unwrap() {
        CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("the fixture owns four inert draft runtimes"),
    });
    (app, source, destination, panes, targets)
}

fn assert_reading_position(app: &Cagents, reading: CardId, fraction: f32) {
    let projection = app.workspace.projection(WIDTH);
    let first_body = projection
        .mounted
        .iter()
        .find(|card| card.start < projection.offset + WIDTH && card.end() > projection.offset)
        .expect("a surviving conversation occupies the viewport");
    assert_eq!(
        first_body.id, reading,
        "the reading pane must remain at the viewport edge: {projection:?}"
    );
    assert_eq!(first_body.visibility, VisibilityClass::Visible);
    let expected = first_body.start + fraction * first_body.width;
    assert!(
        (projection.offset - expected).abs() < 0.001,
        "the reading fraction must survive passive geometry changes: {projection:?}"
    );
    assert!(!app.workspace.camera().is_animating());
    assert_eq!(
        app.workspace.camera().offset(),
        app.workspace.camera().target()
    );
}

fn assert_surviving_reading_after_close_interval(
    app: &mut Cagents,
    reading: CardId,
    selected: CardId,
    fraction: f32,
) {
    // Check after the old from_bounds close animation would have settled as
    // well as the first projected frame. A raw offset can look correct first
    // and silently show the following conversation once that animation ends.
    for elapsed in [
        Duration::ZERO,
        Duration::from_millis(16),
        Duration::from_millis(220),
        Duration::from_secs(1),
    ] {
        let update = app.workspace.tick(elapsed);
        drop(app.apply_viewport(update, false, false));
        assert_eq!(app.workspace.focused(), Some(selected));
        assert_reading_position(app, reading, fraction);
    }
}

fn assert_draft_identity(app: &Cagents, pane: &Pane, target: chat_ui::Target) {
    assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
    match app.cards.get(&pane.id).unwrap() {
        CardRuntime::Chat(runtime) => assert_eq!(runtime.target, target),
        _ => panic!("a surviving draft keeps its runtime"),
    }
}

#[test]
fn a_peer_transfer_before_the_reading_pane_preserves_the_view_after_layout_settles() {
    for select_reading in [false, true] {
        for fraction in [0.0, 0.25] {
            for destination_first in [false, true] {
                let (mut app, source, destination, panes, targets) =
                    four_shared_drafts(select_reading, fraction);
                let selected = app.workspace.focused().unwrap();
                let mut views = app.workspace_model.views.clone();
                views::transfer(
                    &mut views,
                    &Scope::Channel(source),
                    panes[1].id,
                    &Scope::Channel(destination),
                    None,
                )
                .unwrap();
                if destination_first {
                    views.reverse();
                }
                app.workspace_model.views = views;
                app.shared_views.pending.clear();
                drop(app.reconcile_shared_views());

                assert_eq!(
                    app.workspace.order().collect::<Vec<_>>(),
                    [panes[0].id, panes[2].id, panes[3].id]
                );
                assert_eq!(
                    app.local_view_cards(&Scope::Channel(destination)),
                    [panes[1].id]
                );
                assert_surviving_reading_after_close_interval(
                    &mut app,
                    panes[2].id,
                    selected,
                    fraction,
                );
                for (pane, target) in panes.iter().zip(targets) {
                    assert_draft_identity(&app, pane, target);
                }
            }
        }
    }
}

#[test]
fn a_peer_removal_before_the_reading_pane_preserves_the_view_after_layout_settles() {
    for select_reading in [false, true] {
        for fraction in [0.0, 0.25] {
            let (mut app, source, destination, panes, targets) =
                four_shared_drafts(select_reading, fraction);
            let selected = app.workspace.focused().unwrap();
            let source_view = app
                .workspace_model
                .views
                .iter_mut()
                .find(|view| view.scope == Scope::Channel(source))
                .unwrap();
            source_view
                .apply(&Change::Remove { id: panes[1].id })
                .unwrap();
            app.shared_views.pending.clear();
            drop(app.reconcile_shared_views());

            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                [panes[0].id, panes[2].id, panes[3].id]
            );
            assert!(
                app.local_view_cards(&Scope::Channel(destination))
                    .is_empty()
            );
            assert!(!app.card_state(panes[1].id).unwrap().existence.is_open());
            assert!(!app.cards.contains_key(&panes[1].id));
            assert_surviving_reading_after_close_interval(
                &mut app,
                panes[2].id,
                selected,
                fraction,
            );
            for index in [0, 2, 3] {
                assert_draft_identity(&app, &panes[index], targets[index]);
            }
        }
    }
}
