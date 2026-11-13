#![cfg(feature = "ui")]

use cagents::ui_controls::field;
use cosmic::iced::advanced::{
    Layout, Shell, clipboard, layout, mouse, renderer::Headless, widget::Tree,
};
use cosmic::iced::{Event, Font, Pixels, Size};
use cosmic::{Element, Renderer, widget};
use iced_accessibility::{
    A11yId, Id, IdEq,
    accesskit::{Action, ActionData, ActionRequest, NodeId, TreeId},
};

#[test]
fn native_actions_pass_unrelated_buttons_and_reach_only_the_emitted_target() {
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
    let first = Id::new("routing.first");
    let second = Id::new("routing.second");
    let field_id = Id::new("routing.field");
    let mut parent: Element<'_, String> = widget::column::with_children(vec![
        widget::button::text("First")
            .id(first.clone())
            .on_press("first clicked".into())
            .into(),
        widget::button::text("Second")
            .id(second.clone())
            .on_press("second clicked".into())
            .into(),
        field::text("Search settings", "old")
            .id(field_id.clone())
            .on_input(|value| value)
            .into(),
    ])
    .into();
    let mut tree = Tree::new(parent.as_widget());
    let node = parent.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
    );
    let layout = Layout::new(&node);
    let nodes = parent
        .as_widget()
        .a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
    let emitted = |id: Id| {
        let semantic = A11yId::from(id);
        NodeId::from(
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| IdEq::eq(node.id(), &semantic))
                .expect("target is actually emitted")
                .id()
                .clone(),
        )
    };
    let field_target = emitted(field_id);
    let second_target = emitted(second);
    let mut dispatch = |target: NodeId, action: Action, data: Option<ActionData>| {
        let mut messages = Vec::new();
        parent.as_widget_mut().update(
            &mut tree,
            &Event::A11y(
                Id::from(target.0),
                ActionRequest {
                    action,
                    target_tree: TreeId::ROOT,
                    target_node: target,
                    data,
                },
            ),
            layout,
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &layout.bounds(),
        );
        messages
    };
    assert_eq!(
        dispatch(
            field_target,
            Action::SetValue,
            Some(ActionData::Value("native replacement".into()))
        ),
        vec!["native replacement"]
    );
    assert_eq!(
        dispatch(second_target, Action::Click, None),
        vec!["second clicked"]
    );
    assert!(dispatch(NodeId(u64::MAX - 1), Action::Click, None).is_empty());
}

#[test]
fn rebuilt_named_button_keeps_its_published_native_target() {
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
    // Views construct named IDs afresh on every rebuild; each gets a new number.
    let view = || -> Element<'static, &'static str> {
        widget::column::with_children(vec![
            widget::button::text("Other")
                .id(Id::new("rebuild.other"))
                .on_press("other clicked")
                .into(),
            widget::button::text("Target")
                .id(Id::new("rebuild.target"))
                .on_press("target clicked")
                .into(),
        ])
        .into()
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0));
    let published = |element: &mut Element<'_, &'static str>, tree: &mut Tree| {
        let node = element.as_widget_mut().layout(tree, &renderer, &limits);
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), tree, mouse::Cursor::Unavailable);
        let target = A11yId::from(Id::new("rebuild.target"));
        let id = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| IdEq::eq(node.id(), &target))
            .expect("target is emitted")
            .id()
            .clone();
        (NodeId::from(id), node)
    };

    let mut first = view();
    let mut tree = Tree::new(first.as_widget());
    let (before, _) = published(&mut first, &mut tree);

    let mut rebuilt = view();
    tree.diff(rebuilt.as_widget_mut());
    let (after, node) = published(&mut rebuilt, &mut tree);
    assert_eq!(after, before, "the rebuilt control keeps its native ID");

    // An assistive-technology action queued against the earlier frame.
    let layout = Layout::new(&node);
    let mut messages = Vec::new();
    rebuilt.as_widget_mut().update(
        &mut tree,
        &Event::A11y(
            Id::from(before.0),
            ActionRequest {
                action: Action::Click,
                target_tree: TreeId::ROOT,
                target_node: before,
                data: None,
            },
        ),
        layout,
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &layout.bounds(),
    );
    assert_eq!(messages, vec!["target clicked"]);
}

#[test]
fn switches_publish_one_named_state_and_route_native_and_keyboard_activation() {
    use cosmic::iced::advanced::widget::{Operation, operation::Focusable};
    use cosmic::iced::{Rectangle, Vector, keyboard};
    use iced_accessibility::accesskit::{Role, Toggled};

    #[derive(Default)]
    struct FocusSwitch {
        count: usize,
    }
    impl Operation for FocusSwitch {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, _: Option<&widget::Id>, _: Rectangle, state: &mut dyn Focusable) {
            self.count += 1;
            state.focus();
        }
    }

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
    for enabled in [true, false] {
        for checked in [true, false] {
            let id = Id::new("routing.switch");
            let mut element: Element<'_, bool> = field::toggle(checked)
                .id(id.clone())
                .label("Show the status bar".to_owned())
                .on_toggle_maybe(enabled.then_some(|value| value))
                .into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
            );
            let layout = Layout::new(&node).with_virtual_offset(Vector::new(3.0, 7.0));
            let nodes = element
                .as_widget()
                .a11y_nodes(layout, &tree, mouse::Cursor::Unavailable);
            let [switch] = nodes.root().as_slice() else {
                panic!("one switch carries the visible label: {nodes:?}");
            };
            assert!(nodes.children().is_empty());
            assert_eq!(switch.node().role(), Role::Switch);
            assert_eq!(switch.node().label(), Some("Show the status bar"));
            assert_eq!(
                switch.node().toggled(),
                Some(if checked {
                    Toggled::True
                } else {
                    Toggled::False
                })
            );
            assert_eq!(switch.node().is_disabled(), !enabled);
            assert_eq!(switch.node().supports_action(Action::Click), enabled);
            assert_eq!(switch.node().supports_action(Action::Focus), enabled);
            let bounds = switch.node().bounds().unwrap();
            assert_eq!((bounds.x0, bounds.y0), (-3.0, -7.0));
            let target = NodeId::from(switch.id().clone());

            let mut focus = FocusSwitch::default();
            element
                .as_widget_mut()
                .operate(&mut tree, layout, &renderer, &mut focus);
            assert_eq!(focus.count, usize::from(enabled));

            let mut dispatch = |event| {
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &event,
                    layout,
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &layout.bounds(),
                );
                messages
            };
            let native = |target: NodeId| {
                Event::A11y(
                    Id::from(target.0),
                    ActionRequest {
                        action: Action::Click,
                        target_tree: TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                )
            };
            assert!(dispatch(native(NodeId(u64::MAX - 1))).is_empty());
            let expected: Vec<bool> = enabled.then_some(!checked).into_iter().collect();
            assert_eq!(dispatch(native(target)), expected);
            for key in [
                keyboard::Key::Named(keyboard::key::Named::Enter),
                keyboard::Key::Character(" ".into()),
            ] {
                let event = |repeat, modifiers| {
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key.clone(),
                        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                        location: keyboard::Location::Standard,
                        modifiers,
                        text: None,
                        repeat,
                    })
                };
                assert_eq!(
                    dispatch(event(false, keyboard::Modifiers::empty())),
                    expected
                );
                assert!(dispatch(event(true, keyboard::Modifiers::empty())).is_empty());
                assert!(dispatch(event(false, keyboard::Modifiers::CTRL)).is_empty());
            }
        }
    }
}

#[test]
fn disabled_switches_keep_their_checked_color_while_dimming() {
    use cosmic::iced::widget::toggler::{Catalog, Status};

    let theme = <cosmic::Theme as Default>::default();
    let on = Catalog::style(&theme, &(), Status::Active { is_toggled: true });
    let disabled_on = Catalog::style(&theme, &(), Status::Disabled { is_toggled: true });
    let disabled_off = Catalog::style(&theme, &(), Status::Disabled { is_toggled: false });
    assert_eq!(disabled_on.background, on.background.scale_alpha(0.5));
    assert_ne!(disabled_on.background, disabled_off.background);
}

#[test]
fn switches_require_an_owned_press_before_pointer_or_touch_release() {
    use cosmic::iced::{Point, touch, window};

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
    let inside = Point::new(10.0, 10.0);
    let outside = Point::new(500.0, 100.0);
    let mouse_press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
    let mouse_release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
    let touch_press = |id, position| Event::Touch(touch::Event::FingerPressed { id, position });
    let touch_release = |id, position| Event::Touch(touch::Event::FingerLifted { id, position });
    let finger = touch::Finger(1);
    let other_finger = touch::Finger(2);
    let cases = [
        (
            "release without press",
            vec![(mouse_release.clone(), inside)],
            false,
        ),
        (
            "complete mouse click",
            vec![
                (mouse_press.clone(), inside),
                (mouse_release.clone(), inside),
            ],
            true,
        ),
        (
            "mouse press outside",
            vec![
                (mouse_press.clone(), outside),
                (mouse_release.clone(), inside),
            ],
            false,
        ),
        (
            "mouse release outside",
            vec![
                (mouse_press.clone(), inside),
                (mouse_release.clone(), outside),
            ],
            false,
        ),
        (
            "mouse release consumed once",
            vec![
                (mouse_press.clone(), inside),
                (mouse_release.clone(), inside),
                (mouse_release.clone(), inside),
            ],
            true,
        ),
        (
            "window loses focus",
            vec![
                (mouse_press.clone(), inside),
                (Event::Window(window::Event::Unfocused), inside),
                (mouse_release.clone(), inside),
            ],
            false,
        ),
        (
            "pointer leaves window",
            vec![
                (mouse_press, inside),
                (Event::Mouse(mouse::Event::CursorLeft), outside),
                (mouse_release, inside),
            ],
            false,
        ),
        (
            "touch release without press",
            vec![(touch_release(finger, inside), outside)],
            false,
        ),
        (
            "complete touch tap uses touch coordinates",
            vec![
                (touch_press(finger, inside), outside),
                (touch_release(finger, inside), outside),
            ],
            true,
        ),
        (
            "touch starts outside",
            vec![
                (touch_press(finger, outside), inside),
                (touch_release(finger, inside), inside),
            ],
            false,
        ),
        (
            "touch ends outside",
            vec![
                (touch_press(finger, inside), inside),
                (touch_release(finger, outside), inside),
            ],
            false,
        ),
        (
            "unrelated finger cannot release tap",
            vec![
                (touch_press(finger, inside), inside),
                (touch_release(other_finger, inside), inside),
            ],
            false,
        ),
        (
            "touch cancellation clears press",
            vec![
                (touch_press(finger, inside), inside),
                (
                    Event::Touch(touch::Event::FingerLost {
                        id: finger,
                        position: inside,
                    }),
                    inside,
                ),
                (touch_release(finger, inside), inside),
            ],
            false,
        ),
    ];
    for enabled in [true, false] {
        for (name, events, activates) in &cases {
            let mut element: Element<'_, bool> = field::toggle(false)
                .label("Show the status bar".to_owned())
                .on_toggle_maybe(enabled.then_some(|value| value))
                .into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
            );
            let layout = Layout::new(&node);
            let mut messages = Vec::new();
            for (event, pointer) in events {
                element.as_widget_mut().update(
                    &mut tree,
                    event,
                    layout,
                    mouse::Cursor::Available(*pointer),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &layout.bounds(),
                );
            }
            let expected: Vec<bool> = (enabled && *activates)
                .then_some(true)
                .into_iter()
                .collect();
            assert_eq!(messages, expected, "{name}, enabled={enabled}");
        }
    }
}
