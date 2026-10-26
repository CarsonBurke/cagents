#![cfg(feature = "ui")]

//! Exercise the actual COSMIC switch through retained iced state and native events.

use cosmic::iced::advanced::{
    Layout, Shell, clipboard, layout, mouse,
    renderer::Headless,
    widget::{Tree, operation},
};
use cosmic::iced::{Event, Font, Length, Pixels, Point, Size, Vector, keyboard};
use cosmic::{Element, Renderer, widget};
use iced_accessibility::{
    Id, IdEq,
    accesskit::{Action, ActionRequest, NodeId, Rect, Role, Toggled, TreeId},
};

type Message = (u8, bool);

#[test]
fn disabled_switch_style_preserves_the_stored_on_and_off_colors() {
    use cosmic::widget::toggler::{Catalog, Status};

    for theme in [cosmic::Theme::light(), cosmic::Theme::dark()] {
        for is_toggled in [false, true] {
            let active = Catalog::style(&theme, &(), Status::Active { is_toggled });
            let disabled = Catalog::style(&theme, &(), Status::Disabled { is_toggled });
            assert_eq!(disabled.background, active.background.scale_alpha(0.5));
            assert_eq!(disabled.foreground, active.foreground.scale_alpha(0.5));
        }
        let on = Catalog::style(&theme, &(), Status::Disabled { is_toggled: true });
        let off = Catalog::style(&theme, &(), Status::Disabled { is_toggled: false });
        assert_ne!(on.background, off.background);
    }
}

fn switches(first_on: bool, first_enabled: bool) -> Element<'static, Message> {
    widget::column::with_children(vec![
        widget::toggler(first_on)
            .label("First switch".to_owned())
            .width(Length::Fill)
            .on_toggle_maybe(first_enabled.then_some(|on| (1, on)))
            .into(),
        widget::toggler(true)
            .label("Disabled switch".to_owned())
            .width(Length::Fill)
            .into(),
        widget::toggler(true)
            .label("Last switch".to_owned())
            .width(Length::Fill)
            .on_toggle(|on| (3, on))
            .into(),
    ])
    .into()
}

fn key(named: keyboard::key::Named, modifiers: keyboard::Modifiers, repeat: bool) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(named),
        modified_key: keyboard::Key::Named(named),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Space),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    })
}

fn native(target: NodeId, action: Action) -> Event {
    Event::A11y(
        Id::from(target.0),
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: target,
            data: None,
        },
    )
}

fn dispatch(
    parent: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Renderer,
    event: Event,
    cursor: mouse::Cursor,
) -> (Vec<Message>, bool) {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    parent.as_widget_mut().update(
        tree,
        &event,
        Layout::new(node),
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut shell,
        &node.bounds(),
    );
    let captured = shell.is_event_captured();
    (messages, captured)
}

fn operate(
    parent: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &Renderer,
    mut operation: Box<dyn operation::Operation>,
) {
    loop {
        parent
            .as_widget_mut()
            .operate(tree, Layout::new(node), renderer, operation.as_mut());
        match operation.finish() {
            operation::Outcome::Chain(next) => operation = next,
            _ => break,
        }
    }
}

#[test]
fn switches_retain_native_identity_and_route_focus_activation_and_disabled_state() {
    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::DEFAULT,
            Pixels::from(16),
            Some("tiny-skia"),
        ))
        .expect("create a headless renderer");
    let mut parent = switches(false, true);
    let mut tree = Tree::new(parent.as_widget());
    let mut node = parent.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
    );
    let nodes = parent.as_widget().a11y_nodes(
        Layout::new(&node).with_virtual_offset(Vector::new(17.0, 23.0)),
        &tree,
        mouse::Cursor::Unavailable,
    );
    let emitted: Vec<_> = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter(|node| node.node().role() == Role::Switch)
        .collect();
    assert_eq!(emitted.len(), 3);
    for (index, switch) in emitted.iter().enumerate() {
        let semantic = switch.node();
        assert_eq!(
            semantic.label(),
            Some(["First switch", "Disabled switch", "Last switch"][index])
        );
        assert_eq!(semantic.toggled(), Some(Toggled::from(index != 0)));
        assert_eq!(semantic.is_disabled(), index == 1);
        assert_eq!(semantic.supports_action(Action::Focus), index != 1);
        assert_eq!(semantic.supports_action(Action::Click), index != 1);
        let bounds = node.children()[index].bounds();
        assert_eq!(
            semantic.bounds(),
            Some(Rect::new(
                f64::from(bounds.x - 17.0),
                f64::from(bounds.y - 23.0),
                f64::from(bounds.x + bounds.width - 17.0),
                f64::from(bounds.y + bounds.height - 23.0),
            ))
        );
    }
    let ids: Vec<_> = emitted.iter().map(|node| node.id().clone()).collect();
    let targets: Vec<_> = ids.iter().cloned().map(NodeId::from).collect();
    let space = || {
        key(
            keyboard::key::Named::Space,
            keyboard::Modifiers::empty(),
            false,
        )
    };

    // An unfocused switch never consumes the editor's Space key.
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![], false)
    );
    operate(
        &mut parent,
        &mut tree,
        &node,
        &renderer,
        Box::new(operation::focusable::focus_next::<()>()),
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![(1, true)], true)
    );
    for (modifiers, repeat, captured) in [
        (keyboard::Modifiers::CTRL, false, false),
        (keyboard::Modifiers::empty(), true, true),
    ] {
        assert_eq!(
            dispatch(
                &mut parent,
                &mut tree,
                &node,
                &renderer,
                key(keyboard::key::Named::Space, modifiers, repeat),
                mouse::Cursor::Unavailable
            ),
            (vec![], captured)
        );
    }

    // Rebuild and reflow while focused: IDs and focus belong to retained state.
    parent = switches(true, true);
    tree.diff(parent.as_widget_mut());
    node = parent.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(220.0, 180.0)),
    );
    let rebuilt =
        parent
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let rebuilt: Vec<_> = rebuilt
        .root()
        .iter()
        .chain(rebuilt.children())
        .filter(|node| node.node().role() == Role::Switch)
        .collect();
    for (before, after) in ids.iter().zip(&rebuilt) {
        assert!(IdEq::eq(before, after.id()));
    }
    assert_eq!(rebuilt[0].node().toggled(), Some(Toggled::True));
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![(1, false)], true)
    );

    // Tab and Shift+Tab use the real focus operations and skip disabled switches.
    operate(
        &mut parent,
        &mut tree,
        &node,
        &renderer,
        Box::new(operation::focusable::focus_next::<()>()),
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![(3, false)], true)
    );
    operate(
        &mut parent,
        &mut tree,
        &node,
        &renderer,
        Box::new(operation::focusable::focus_previous::<()>()),
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            key(
                keyboard::key::Named::Enter,
                keyboard::Modifiers::empty(),
                false
            ),
            mouse::Cursor::Unavailable
        ),
        (vec![(1, false)], true)
    );

    // Native actions use actual advertised IDs, even with no pointer over the row.
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            native(targets[2], Action::Click),
            mouse::Cursor::Unavailable
        ),
        (vec![(3, false)], true)
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            native(NodeId(u64::MAX - 1), Action::Click),
            mouse::Cursor::Unavailable
        ),
        (vec![], false)
    );
    operate(
        &mut parent,
        &mut tree,
        &node,
        &renderer,
        Box::new(operation::focusable::unfocus::<()>()),
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            native(targets[2], Action::Focus),
            mouse::Cursor::Unavailable
        ),
        (vec![], true)
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![(3, false)], true)
    );
    for action in [Action::Focus, Action::Click] {
        assert_eq!(
            dispatch(
                &mut parent,
                &mut tree,
                &node,
                &renderer,
                native(targets[1], action),
                mouse::Cursor::Unavailable
            ),
            (vec![], false)
        );
    }

    // Ordinary pointer release keeps its existing activation contract.
    for (index, expected) in [(0, vec![(1, false)]), (1, vec![])] {
        let bounds = node.children()[index].bounds();
        assert_eq!(
            dispatch(
                &mut parent,
                &mut tree,
                &node,
                &renderer,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                mouse::Cursor::Available(Point::new(bounds.center_x(), bounds.center_y()))
            )
            .0,
            expected
        );
        assert_eq!(
            parent.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(bounds.center()),
                &node.bounds(),
                &renderer
            ),
            if index == 0 {
                mouse::Interaction::Pointer
            } else {
                mouse::Interaction::None
            }
        );
    }

    // Disabling a focused switch clears its retained focus and blocks old IDs.
    operate(
        &mut parent,
        &mut tree,
        &node,
        &renderer,
        Box::new(operation::focusable::focus::<()>(Id::from(targets[0].0))),
    );
    parent = switches(true, false);
    tree.diff(parent.as_widget_mut());
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            space(),
            mouse::Cursor::Unavailable
        ),
        (vec![], false)
    );
    assert_eq!(
        dispatch(
            &mut parent,
            &mut tree,
            &node,
            &renderer,
            native(targets[0], Action::Click),
            mouse::Cursor::Unavailable
        ),
        (vec![], false)
    );
}
