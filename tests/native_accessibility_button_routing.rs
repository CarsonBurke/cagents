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
