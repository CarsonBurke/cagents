use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId};

fn popover_button(
    tree: &iced_accessibility::A11yTree,
    label: &str,
) -> iced_accessibility::A11yNode {
    let buttons = tree
        .root()
        .iter()
        .chain(tree.children())
        .filter(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() == Some(label)
                && !node.node().is_disabled()
                && !node.node().is_hidden()
        })
        .collect::<Vec<_>>();
    assert_eq!(buttons.len(), 1, "one current enabled button: {label}");
    (*buttons[0]).clone()
}

fn popover_click(node: &iced_accessibility::A11yNode) -> iced::Event {
    use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, TreeId};
    let target_node = NodeId::from(node.id().clone());
    iced::Event::A11y(
        iced::advanced::widget::Id::from(u128::from(target_node.0) as u64),
        ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node,
            data: None,
        },
    )
}

#[test]
fn whole_application_popover_exports_current_discard_controls_and_cancel_reduces() {
    use iced::{
        Size,
        advanced::clipboard,
        mouse,
        runtime::user_interface::{Cache, UserInterface},
    };
    use iced_accessibility::accesskit::Role;

    fn frame(
        app: &Cagents,
        cache: Cache,
        click: Option<&str>,
    ) -> (Cache, iced_accessibility::A11yTree, Vec<Message>) {
        let mut renderer = tests::headless_renderer();
        // This includes COSMIC's real dialog popover, not app.dialog() alone.
        let mut ui = UserInterface::build(
            cosmic::ApplicationExt::view_main(app),
            Size::new(1280.0, 900.0),
            cache,
            &mut renderer,
        );
        let mut messages = Vec::new();
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.is_empty());
        // Draw reconstructs the overlay and uses its retained real layout.
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        if let Some(label) = click {
            let button = popover_button(&tree, label);
            let bounds = button.node().bounds().expect("actual dialog geometry");
            assert!(bounds.x0 >= 0.0 && bounds.x1 <= 1280.0 && bounds.x1 > bounds.x0);
            assert!(bounds.y0 >= 0.0 && bounds.y1 <= 900.0 && bounds.y1 > bounds.y0);
            let (_, statuses) = ui.update(
                &[popover_click(&button)],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(statuses, vec![iced::event::Status::Captured]);
        }
        let messages = messages
            .into_iter()
            .map(|message| {
                let cosmic::Action::App(message) = message else {
                    panic!("unexpected COSMIC action")
                };
                message
            })
            .collect();
        (ui.into_cache(), tree, messages)
    }

    let mut app = tests::context_inspector_test_app(false);
    let (mut cache, base, messages) = frame(&app, Cache::default(), None);
    assert!(messages.is_empty());
    assert!(
        base.root()
            .iter()
            .chain(base.children())
            .any(|node| node.node().role() == Role::Button)
    );
    assert!(
        !base
            .root()
            .iter()
            .chain(base.children())
            .any(|node| node.node().label() == Some("Discard changes?"))
    );

    // Rediff the same real dialog with a different owner and different paths.
    // A retained layout/Tree must never publish the previous closure's action.
    for round in 0..2 {
        if round != 0 {
            let (next, _, mut messages) = frame(&app, cache, Some("Cancel"));
            cache = next;
            assert_eq!(messages.len(), 1);
            let cancel = messages.pop().unwrap();
            assert!(matches!(
                cancel,
                Message::Sidebar(SidebarAction::CancelDiscard)
            ));
            drop(app.update(cancel));
            assert!(!app.modal_is_open());
        }
        let key = ActiveWorktreeKey {
            project_id: ProjectId::default(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: PathBuf::from(format!("/fixture-owner-{round}")),
        };
        let request = crate::repository::DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: [format!("modified-{round}"), format!("deleted-{round}")]
                .into_iter()
                .map(|path| crate::repository::PathSelection::new(RepoPath::parse(path).unwrap()))
                .collect(),
            untracked: Vec::new(),
            confirmed: false,
        };
        drop(app.update(Message::Sidebar(SidebarAction::RequestDiscard {
            key: key.clone(),
            request: request.clone(),
        })));
        assert!(app.modal_is_open());
        let (next, modal, mut messages) = frame(&app, cache, Some("Discard"));
        cache = next;
        let labels = modal
            .root()
            .iter()
            .chain(modal.children())
            .filter(|node| node.node().role() == Role::Button)
            .map(|node| node.node().label().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["Cancel", "Discard"],
            "modal must not expose background controls"
        );
        assert!(
            modal
                .root()
                .iter()
                .chain(modal.children())
                .any(|node| node.node().label()
                    == Some("2 tracked paths will be restored from the index"))
        );
        assert_eq!(messages.len(), 1);
        let Message::Sidebar(SidebarAction::ConfirmDiscard {
            key: actual,
            request: confirmed,
        }) = messages.pop().unwrap()
        else {
            panic!("actual popup did not deliver its confirmation")
        };
        assert_eq!(actual, key);
        assert_eq!(
            confirmed,
            crate::repository::DiscardRequest {
                confirmed: true,
                ..request
            }
        );
        // Inspect the actual owning confirmation without executing a mutation.
        assert!(app.source_control_mutations.is_empty());
    }
    let (next, _, mut messages) = frame(&app, cache, Some("Cancel"));
    cache = next;
    assert_eq!(messages.len(), 1);
    let cancel = messages.pop().unwrap();
    assert!(matches!(
        cancel,
        Message::Sidebar(SidebarAction::CancelDiscard)
    ));
    drop(app.update(cancel));
    assert!(!app.modal_is_open());
    assert!(app.sidebar_state.pending_discard().is_none());
    assert!(app.source_control_mutations.is_empty());
    let (_, after, messages) = frame(&app, cache, None);
    assert!(messages.is_empty());
    assert!(
        after
            .root()
            .iter()
            .chain(after.children())
            .any(|node| node.node().role() == Role::Button)
    );
    assert!(
        !after
            .root()
            .iter()
            .chain(after.children())
            .any(|node| node.node().label() == Some("Discard changes?"))
    );
}

#[test]
fn popover_accessibility_uses_actual_translated_layout_and_current_modal_tree() {
    use iced::{
        Point, Size,
        advanced::clipboard,
        mouse,
        runtime::user_interface::{Cache, UserInterface},
    };

    fn element(
        popup: bool,
        modal: bool,
        point: Point,
        base_id: &widget::Id,
        popup_id: &widget::Id,
    ) -> Element<'static, u8> {
        let base = widget::button::custom(design::caption("Base action"))
            .id(base_id.clone())
            .name("Base action")
            .width(Length::Fixed(400.0))
            .height(Length::Fixed(300.0))
            .on_press(1);
        let mut view = widget::popover(base)
            .modal(modal)
            .position(widget::popover::Position::Point(point));
        if popup {
            view = view.popup(
                widget::button::custom(design::caption("Popup action"))
                    .id(popup_id.clone())
                    .name("Popup action")
                    .width(Length::Fixed(120.0))
                    .height(Length::Fixed(40.0))
                    .on_press(2),
            );
        }
        widget::container(view).padding(20).into()
    }
    let mut renderer = tests::headless_renderer();
    // Ordinary caller-owned IDs; do not relabel emitted accessibility nodes.
    let base_id = widget::Id::new("popover.contract.base");
    let popup_id = widget::Id::new("popover.contract.popup");
    let bounds = Size::new(500.0, 400.0);
    let mut messages = Vec::new();
    let mut ui = UserInterface::build(
        element(true, false, Point::new(75.0, 100.0), &base_id, &popup_id),
        bounds,
        Cache::default(),
        &mut renderer,
    );
    let before = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let base = popover_button(&before, "Base action");
    assert!(
        !before
            .root()
            .iter()
            .chain(before.children())
            .any(|node| node.node().label() == Some("Popup action")),
        "a popup without an actual overlay layout must not be fabricated"
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let accessible = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let popup = popover_button(&accessible, "Popup action");
    assert_eq!(
        popover_button(&accessible, "Base action").node().bounds(),
        base.node().bounds()
    );
    let rect = popup.node().bounds().unwrap();
    assert_eq!(
        (rect.x0, rect.y0, rect.x1, rect.y1),
        (95.0, 120.0, 215.0, 160.0)
    );
    let ids = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .map(|node| node.id().clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![base.id().clone(), popup.id().clone()],
        "nonmodal stacking follows base then popup"
    );
    ui.draw(
        &mut renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    assert_eq!(
        popover_button(&ui.a11y_nodes(mouse::Cursor::Unavailable), "Popup action"),
        popup,
        "draw may reuse the actual overlay layout without erasing it"
    );
    ui.update(
        &[popover_click(&popup)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![2]);
    let mut cache = ui.into_cache();

    // Rebuild and resize twice; each new frame must replace the old geometry,
    // then ordinary overlay layout must make the same current control visible.
    for (resized, expected_native) in [
        // Point placement flips an overflowing context menu UP by its own
        // height before clamping. It does not always hug the bottom edge.
        (bounds, (380.0, 330.0, 500.0, 370.0)),
        (Size::new(420.0, 320.0), (300.0, 280.0, 420.0, 320.0)),
    ] {
        // Obtain the native drawing/event layout independently of AX export
        // and of the UserInterface/cache under test. Exact equality below
        // checks AX against the actual owning overlay, not guessed offsets.
        let native_bounds = {
            let mut reference = element(true, true, Point::new(450.0, 350.0), &base_id, &popup_id);
            let mut reference_tree = iced::advanced::widget::Tree::new(reference.as_widget());
            let base_layout = reference.as_widget_mut().layout(
                &mut reference_tree,
                &renderer,
                &iced::advanced::layout::Limits::new(Size::ZERO, resized),
            );
            let mut overlay = reference
                .as_widget_mut()
                .overlay(
                    &mut reference_tree,
                    iced::advanced::Layout::new(&base_layout),
                    &renderer,
                    &iced::Rectangle::with_size(resized),
                    iced::Vector::ZERO,
                )
                .expect("the actual widget owns a popup overlay");
            let node = overlay.as_overlay_mut().layout(&renderer, resized);
            let rect = iced::advanced::Layout::new(&node).bounds();
            assert_eq!(
                (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height),
                expected_native,
                "native Point placement retains its context-menu flip semantics"
            );
            rect
        };
        let mut ui = UserInterface::build(
            element(true, true, Point::new(450.0, 350.0), &base_id, &popup_id),
            resized,
            cache,
            &mut renderer,
        );
        assert!(ui.a11y_nodes(mouse::Cursor::Unavailable).root().is_empty());
        messages.clear();
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let current = popover_button(&tree, "Popup action");
        assert_eq!(
            current.id(),
            popup.id(),
            "ordinary rediff must preserve native identity"
        );
        let rect = current.node().bounds().unwrap();
        assert_eq!(
            (rect.x0, rect.y0, rect.x1, rect.y1),
            (
                f64::from(native_bounds.x),
                f64::from(native_bounds.y),
                f64::from(native_bounds.x + native_bounds.width),
                f64::from(native_bounds.y + native_bounds.height),
            )
        );
        assert!(!tree.contains(base.id()));
        let (_, statuses) = ui.update(
            &[popover_click(&base)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, vec![iced::event::Status::Captured]);
        assert!(
            messages.is_empty(),
            "a stale background target must not escape the modal"
        );
        ui.update(
            &[popover_click(&current)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![2]);
        cache = ui.into_cache();
    }
    let mut ui = UserInterface::build(
        element(false, true, Point::ORIGIN, &base_id, &popup_id),
        bounds,
        cache,
        &mut renderer,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let current_base = popover_button(&tree, "Base action");
    assert!(
        !tree.contains(popup.id()),
        "removed popup nodes must not survive in AX"
    );
    messages.clear();
    ui.update(
        &[popover_click(&popup)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.is_empty(),
        "a removed popup target must not activate the base"
    );
    ui.update(
        &[popover_click(&current_base)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![1]);
}

#[test]
fn nested_modal_popover_accessibility_owns_foreground_and_rejects_ancestor_actions() {
    use iced::{
        Point, Size,
        advanced::clipboard,
        mouse,
        runtime::user_interface::{Cache, UserInterface},
    };

    fn element(
        popup: bool,
        modal: bool,
        action: u8,
        point: Point,
        ids: &[widget::Id; 4],
        peer_modal: Option<bool>,
    ) -> Element<'static, u8> {
        let button = |label: &'static str, index: usize, action| {
            widget::button::custom(design::caption(label))
                .id(ids[index].clone())
                .name(label)
                .width(Length::Fixed(160.0))
                .height(Length::Fixed(40.0))
                .on_press(action)
        };
        let mut inner = widget::popover(button("Inner base B", 1, 2))
            .modal(modal)
            .position(widget::popover::Position::Point(point));
        if popup {
            inner = inner.popup(widget::container(button("Foreground C", 2, action)).padding(8));
        }
        // A is not inside the inner Popover's base. Blocking only that base
        // cannot protect A when the nested overlay refuses its stale AX ID.
        let mut foreground = widget::column::with_children(vec![
            button("Ancestor sibling A", 0, 1).into(),
            inner.into(),
        ]);
        if let Some(modal) = peer_modal {
            foreground = foreground.push(
                widget::popover(design::caption("Peer base"))
                    .modal(modal)
                    .popup(button("Peer foreground D", 3, 9)),
            );
        }
        widget::popover(design::caption("Outer base"))
            .modal(true)
            .popup(foreground)
            .into()
    }

    let ids = [
        widget::Id::new("popover.nested.a"),
        widget::Id::new("popover.nested.b"),
        widget::Id::new("popover.nested.c"),
        widget::Id::new("popover.nested.d"),
    ];
    let bounds = Size::new(640.0, 480.0);
    let mut renderer = tests::headless_renderer();
    let mut messages = Vec::new();
    let mut ui = UserInterface::build(
        element(false, true, 3, Point::ORIGIN, &ids, None),
        bounds,
        Cache::default(),
        &mut renderer,
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let initial = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let a = popover_button(&initial, "Ancestor sibling A");
    let b = popover_button(&initial, "Inner base B");
    let mut cache = ui.into_cache();
    let mut previous_bounds = None;
    let mut last_c = None;

    for (action, point) in [(3, Point::ORIGIN), (4, Point::new(60.0, 80.0))] {
        let mut ui = UserInterface::build(
            element(true, true, action, point, &ids, None),
            bounds,
            cache,
            &mut renderer,
        );
        messages.clear();
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let c = popover_button(&tree, "Foreground C");
        assert_eq!(
            popover_button(&ui.a11y_nodes(mouse::Cursor::Unavailable), "Foreground C"),
            c,
            "repeated AX export must not retain stale roots or erase current ones"
        );
        assert!(
            !tree.contains(a.id()),
            "ancestor sibling is behind the nested modal"
        );
        assert!(
            !tree.contains(b.id()),
            "inner base is behind the nested modal"
        );
        assert!(
            tree.root().iter().any(|node| node.id() == c.id()),
            "foreground roots must survive ancestor AX containers"
        );
        let rect = c.node().bounds().unwrap();
        assert!(rect.x0 >= 0.0 && rect.y0 >= 0.0);
        assert!(rect.x1 <= 640.0 && rect.y1 <= 480.0);
        assert!(rect.x1 > rect.x0 && rect.y1 > rect.y0);
        if let Some(previous) = previous_bounds {
            assert_ne!(rect, previous, "rediff must replace nested popup geometry");
        }
        previous_bounds = Some(rect);
        let (_, statuses) = ui.update(
            &[popover_click(&a), popover_click(&b)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, vec![iced::event::Status::Captured; 2]);
        assert!(
            messages.is_empty(),
            "stale ancestor/base targets must publish nothing"
        );
        ui.update(
            &[popover_click(&c)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![action], "current foreground owns the action");
        last_c = Some(c);
        cache = ui.into_cache();
    }

    // With a nonmodal nested popup the ancestor sibling and inner base remain
    // ordinary enabled controls, alongside the popup's current action.
    let mut ui = UserInterface::build(
        element(true, false, 5, Point::ORIGIN, &ids, None),
        bounds,
        cache,
        &mut renderer,
    );
    messages.clear();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let current_a = popover_button(&tree, "Ancestor sibling A");
    let current_b = popover_button(&tree, "Inner base B");
    let current_c = popover_button(&tree, "Foreground C");
    ui.update(
        &[
            popover_click(&current_a),
            popover_click(&current_b),
            popover_click(&current_c),
        ],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![1, 2, 5]);
    cache = ui.into_cache();

    let mut ui = UserInterface::build(
        element(false, true, 6, Point::ORIGIN, &ids, None),
        bounds,
        cache,
        &mut renderer,
    );
    messages.clear();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let current_a = popover_button(&tree, "Ancestor sibling A");
    let current_b = popover_button(&tree, "Inner base B");
    assert!(!tree.contains(last_c.as_ref().unwrap().id()));
    ui.update(
        &[popover_click(last_c.as_ref().unwrap())],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.is_empty(),
        "removed nested popup cannot activate another owner"
    );
    ui.update(
        &[popover_click(&current_a), popover_click(&current_b)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![1, 2]);

    // Group forwards AX to every overlay child even after capture. Layout
    // custody must select one actual modal foreground and guard its peer too.
    cache = ui.into_cache();
    let mut ui = UserInterface::build(
        element(true, false, 8, Point::ORIGIN, &ids, Some(false)),
        bounds,
        cache,
        &mut renderer,
    );
    messages.clear();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let both = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let c = popover_button(&both, "Foreground C");
    let d = popover_button(&both, "Peer foreground D");
    cache = ui.into_cache();
    let mut ui = UserInterface::build(
        element(true, true, 8, Point::ORIGIN, &ids, Some(false)),
        bounds,
        cache,
        &mut renderer,
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let modal_c = popover_button(&tree, "Foreground C");
    let above_d = popover_button(&tree, "Peer foreground D");
    ui.draw(
        &mut renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let after_layout = ui.a11y_nodes(mouse::Cursor::Unavailable);
    assert_eq!(popover_button(&after_layout, "Foreground C"), modal_c);
    assert_eq!(popover_button(&after_layout, "Peer foreground D"), above_d);
    assert!(!tree.contains(a.id()));
    assert!(!tree.contains(b.id()));
    ui.update(
        &[popover_click(&a)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(messages.is_empty());
    ui.update(
        &[popover_click(&modal_c), popover_click(&above_d)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(
        messages,
        vec![8, 9],
        "a nonmodal popup above the boundary stays usable"
    );
    cache = ui.into_cache();
    messages.clear();
    let mut ui = UserInterface::build(
        element(true, true, 8, Point::ORIGIN, &ids, Some(true)),
        bounds,
        cache,
        &mut renderer,
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let controls = tree
        .root()
        .iter()
        .chain(tree.children())
        .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
        .collect::<Vec<_>>();
    assert_eq!(
        controls.len(),
        1,
        "one actual modal overlay owns this group"
    );
    let active = controls[0];
    let (stale, expected) = if active.id() == c.id() {
        (&d, 8)
    } else {
        assert_eq!(active.id(), d.id());
        (&c, 9)
    };
    assert!(!tree.contains(stale.id()));
    ui.update(
        &[popover_click(stale)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        messages.is_empty(),
        "a modal peer must not publish after group capture"
    );
    ui.update(
        &[popover_click(active)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![expected]);

    // Rediff/layout/export withdraw the removed higher popup's IDs. Its old
    // action and a wholly unknown native target must remain blocked, while
    // the current modal control still dispatches exactly once.
    cache = ui.into_cache();
    messages.clear();
    let mut ui = UserInterface::build(
        element(true, true, 10, Point::ORIGIN, &ids, None),
        bounds,
        cache,
        &mut renderer,
    );
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    assert!(!tree.contains(d.id()));
    let current = popover_button(&tree, "Foreground C");
    let unknown = iced_accessibility::accesskit::NodeId(u64::MAX - 1);
    let unknown_click = iced::Event::A11y(
        widget::Id::from(unknown.0),
        iced_accessibility::accesskit::ActionRequest {
            action: iced_accessibility::accesskit::Action::Click,
            target_tree: iced_accessibility::accesskit::TreeId::ROOT,
            target_node: unknown,
            data: None,
        },
    );
    let (_, statuses) = ui.update(
        &[popover_click(&d), unknown_click],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(statuses, vec![iced::event::Status::Captured; 2]);
    assert!(messages.is_empty());
    ui.update(
        &[popover_click(&current)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![10]);
}

fn live_app() -> (Cagents, CardId, TerminalId, HostSessionProof) {
    let mut app = tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    let card = app.workspace.focused().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("terminal fixture");
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let terminal = session.id;
    (app, card, terminal, proof)
}

fn report_close(app: &mut Cagents, outcome: terminal_close::CloseOutcome) {
    let intent = app.terminal_close.as_ref().unwrap().clone();
    let _ = app.update(Message::TerminalCloseFinished {
        card: intent.card,
        terminal: intent.terminal,
        proof: intent.proof,
        generation: intent.generation,
        outcome,
    });
}

/// Reopening a closed pane runs its saved command again without asking. That
/// is an undo for a pane someone just stopped, but a dead one has nothing to
/// undo, and clearing many of them mustn't leave a stack of launches behind.
#[test]
fn only_a_terminal_that_was_running_can_be_reopened() {
    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    report_close(&mut app, terminal_close::CloseOutcome::NotHosted);
    let _ = app.update(Message::CommitClose(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(app.recently_closed.is_empty());
    assert!(app.proven_gone_cards.is_empty());

    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    report_close(&mut app, terminal_close::CloseOutcome::Closed);
    let _ = app.update(Message::CommitClose(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert_eq!(app.recently_closed.len(), 1);
}

#[test]
fn a_terminal_proven_gone_leaves_through_ordinary_removal() {
    let (mut app, card, terminal, _) = live_app();
    app.set_residency(card, Residency::Hot);
    let _ = app.update(Message::CloseCard(card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.terminal_close.as_ref().unwrap().pending);
    report_close(&mut app, terminal_close::CloseOutcome::NotHosted);
    assert!(app.terminal_close.is_none());
    assert!(!app.terminal_close_attempts.contains(&terminal));
    let _ = app.update(Message::CommitClose(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn a_proof_of_absence_cannot_retire_a_pane_that_changed_or_is_starting() {
    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    // Any change, even one keeping the identity and proof, means the proof was
    // about a pane that no longer exists.
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.resume_hint = Some("changed while closing".into());
    report_close(&mut app, terminal_close::CloseOutcome::NotHosted);
    assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.workspace.pending_close().is_none());

    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    app.begin_terminal_start(card);
    report_close(&mut app, terminal_close::CloseOutcome::NotHosted);
    assert!(
        app.terminal_close
            .as_ref()
            .unwrap()
            .detail
            .as_deref()
            .unwrap()
            .contains("starting")
    );
    assert!(app.workspace.pending_close().is_none());
}

#[test]
fn a_kept_terminal_says_why_and_allows_a_fresh_explicit_close() {
    let (mut app, card, terminal, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    report_close(
        &mut app,
        terminal_close::CloseOutcome::Kept("the reason to keep it".into()),
    );
    let intent = app.terminal_close.as_ref().unwrap();
    assert_eq!(intent.detail.as_deref(), Some("the reason to keep it"));
    assert!(!intent.pending);
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.terminal_close_attempts.contains(&terminal));
    let _ = app.update(Message::CancelTerminalClose);
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close.as_ref().unwrap().pending);
}

/// #2365: a pane kept only by processes Close couldn't read offers to close
/// anyway. Keeping it stays the focused choice; closing proves everything again
/// and passes over exactly the processes the dialog named.
#[test]
fn a_pane_kept_by_unreadable_processes_can_be_closed_beside_them() {
    use crate::upgrade::UnreadableProcess;
    let (mut app, card, terminal, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    let agent = UnreadableProcess::fixture(4242, 100, "ssh-agent");
    report_close(
        &mut app,
        terminal_close::CloseOutcome::Kept(super::daemon_recovery::KeptPane {
            reason: "ssh-agent (PID 4242) can't be read".into(),
            unreadable: vec![agent.clone()],
        }),
    );
    assert!(app.terminal_close_dialog_visible());
    assert!(app.card_state(card).unwrap().existence.is_open());

    let _ = app.update(Message::ConfirmTerminalClose);
    let intent = app.terminal_close.as_ref().unwrap();
    assert!(intent.pending, "Close anyway starts a fresh close");
    assert!(intent.detail.is_none());
    assert!(
        app.terminal_close_dialog_visible(),
        "the dialog shows the check"
    );
    assert!(app.terminal_close_attempts.contains(&terminal));
    // A second press while it checks does nothing.
    let _ = app.update(Message::ConfirmTerminalClose);

    // Something else turns up: that is reported, and nothing else is offered.
    report_close(
        &mut app,
        terminal_close::CloseOutcome::Kept("A process started by this terminal".into()),
    );
    let intent = app.terminal_close.as_ref().unwrap();
    assert!(!intent.pending);
    assert_eq!(
        intent.detail.as_deref(),
        Some("A process started by this terminal")
    );
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(
        !app.terminal_close.as_ref().unwrap().pending,
        "a refusal that names no unreadable process can't be closed anyway"
    );

    // Without the choice, a kept pane is only dismissed.
    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.terminal_close.is_none());
    assert!(app.card_state(card).unwrap().existence.is_open());
}

/// A refusal takes over from the wait, and the pane can be closed again.
#[test]
fn a_pane_says_its_close_is_being_checked_only_until_the_answer() {
    let (mut app, card, _, _) = live_app();
    assert!(!app.terminal_close_in_progress(card));
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close_in_progress(card));
    report_close(
        &mut app,
        terminal_close::CloseOutcome::Kept("The pane was kept.".into()),
    );
    assert!(!app.terminal_close_in_progress(card));
}

/// A lost connection used to stop Close before it asked anything, so a pane
/// whose daemon came back without its terminal could never be removed.
#[test]
fn a_lost_connection_only_asks_whether_the_terminal_is_gone() {
    let (mut app, card, _, proof) = live_app();
    app.uncertain_terminal_proofs.insert(card, proof);
    let _ = app.update(Message::CloseCard(card));
    let intent = app.terminal_close.as_ref().unwrap();
    assert!(intent.pending);
    assert!(intent.is_probe_only());
    assert!(intent.detail.is_none());
    report_close(&mut app, terminal_close::CloseOutcome::NotHosted);
    let _ = app.update(Message::CommitClose(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());

    // Without a saved identity there is nothing to ask about.
    let (mut app, card, _, proof) = live_app();
    app.uncertain_terminal_proofs.insert(card, proof);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Failed {
        typed_error: crate::model::RuntimeError {
            code: "runtime_failed".into(),
            message: "failed".into(),
        },
        retryable: true,
    };
    let _ = app.update(Message::CloseCard(card));
    let intent = app.terminal_close.as_ref().unwrap();
    assert!(!intent.pending);
    assert!(intent.detail.as_deref().unwrap().contains("Reconnect"));
}

fn request_active_close(app: &mut Cagents, card: CardId) {
    let _ = app.update(Message::CloseCard(card));
    report_close(app, terminal_close::CloseOutcome::Active);
}

#[test]
fn pointer_and_keyboard_close_preserve_live_card_until_confirmation() {
    for keyboard in [false, true] {
        let (mut app, card, _, _) = live_app();
        let message = if keyboard {
            close_focused_message(&app.workspace).unwrap()
        } else {
            Message::CloseCard(card)
        };
        let _ = app.update(message);
        assert!(app.terminal_close_dialog().is_none());
        report_close(&mut app, terminal_close::CloseOutcome::Active);
        assert!(app.terminal_close.is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.terminal_close_attempts.is_empty());
        let _ = app.update(Message::CancelTerminalClose);
        assert!(app.terminal_close.is_none());
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(app.terminal_close_attempts.is_empty());
    }
}

/// Escape keeps the terminal, even when the terminal the question opened over
/// still has focus and captured the key.
#[test]
fn escape_consumes_terminal_confirmation_without_closing_context() {
    for captured in [false, true] {
        let (mut app, card, _, _) = live_app();
        request_active_close(&mut app, card);
        let key = keyboard::Key::Named(keyboard::key::Named::Escape);
        let _ = app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured,
        });
        assert!(app.terminal_close.is_none(), "captured={captured}");
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.terminal_close_attempts.is_empty());
        assert_eq!(app.workspace.focused(), Some(card));
    }
}

#[test]
fn exit_during_confirmation_releases_completed_holder_and_proof_change_keeps_card() {
    let (mut app, card, _, _) = live_app();
    app.set_residency(card, Residency::Hot);
    request_active_close(&mut app, card);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let terminal = session.id;
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.terminal_close.as_ref().unwrap().pending);
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.closed_terminal_cards.contains(&card));
    assert!(app.terminal_close_attempts.contains(&terminal));
    report_close(&mut app, terminal_close::CloseOutcome::Closed);
    assert!(app.closed_terminal_cards.contains(&card));
    assert!(app.terminal_close.is_none());

    let (mut app, card, _, _) = live_app();
    request_active_close(&mut app, card);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    if let TerminalContinuity::Live {
        host_incarnation, ..
    } = &mut session.continuity
    {
        *host_incarnation = HostIncarnation::new("restarted");
    }
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
    assert!(app.terminal_close_attempts.is_empty());
    assert!(app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn unreachable_and_duplicate_completions_never_remove_or_resend() {
    let (mut app, card, terminal, proof) = live_app();
    let _ = app.update(Message::CloseCard(card));
    let _pending = app.update(Message::ConfirmTerminalClose); // Not polled: never touches a real daemon.
    let _duplicate = app.update(Message::ConfirmTerminalClose);
    assert_eq!(app.terminal_close_attempts.len(), 1);
    let generation = app.terminal_close.as_ref().unwrap().generation;
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal,
        proof: proof.clone(),
        generation,
        outcome: terminal_close::CloseOutcome::Unreachable("disconnected".into()),
    });
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal,
        proof,
        generation,
        outcome: terminal_close::CloseOutcome::Closed,
    });
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.closed_terminal_cards.contains(&card));
    assert_eq!(app.terminal_close_attempts.len(), 1);
}

fn github_key() -> GitHubRepoKey {
    GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap()
}

/// A project whose last completed refresh confirmed `key`, which is the only
/// thing that ever grants authority to mutate.
fn authorized_project(app: &mut Cagents, key: &GitHubRepoKey) -> ProjectId {
    let project = ProjectId::new();
    app.repository_generations.insert(project, 1);
    app.github_availability.insert(
        project,
        GitHubAvailability::Ready {
            repository_key: key.clone(),
            pull_requests: Vec::new(),
            refreshed_at: 1,
        },
    );
    project
}

fn open_draft(app: &mut Cagents, project: ProjectId) {
    let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
        project_id: project,
        worktree: None,
    }));
    let dialog = app
        .pull_request_dialog
        .as_mut()
        .expect("a confirmed repository authorizes a draft");
    dialog.head = "topic".into();
    dialog.base = "trunk".into();
    dialog.title = "Authored title".into();
}

fn confirmed_repository(app: &Cagents, project: ProjectId) -> Option<&GitHubRepoKey> {
    app.github_availability
        .get(&project)
        .and_then(GitHubAvailability::repository_key)
}

#[test]
fn every_completed_degraded_github_refresh_revokes_prior_dialog_authority() {
    for unavailable in [
        GitHubAvailability::MissingCli,
        GitHubAvailability::UnsupportedCli,
        GitHubAvailability::Unauthenticated,
        GitHubAvailability::NotAssociated,
        GitHubAvailability::Failed {
            detail: "offline".into(),
        },
    ] {
        let mut app = tests::context_inspector_test_app(false);
        let key = github_key();
        let project = authorized_project(&mut app, &key);
        open_draft(&mut app, project);
        app.github_availability.insert(project, unavailable);
        let _ = app.update(Message::SubmitPullRequest);
        assert!(
            app.pull_request_dialog.is_some(),
            "a revoked draft was submitted"
        );
        let _ = app.update(Message::CancelDialog);
        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id: project,
            worktree: None,
        }));
        assert!(
            app.pull_request_dialog.is_none(),
            "a revoked identity opened a mutation"
        );
    }
}

#[test]
fn an_in_flight_refresh_neither_revokes_nor_reauthorizes_authority() {
    let mut app = tests::context_inspector_test_app(false);
    let key = github_key();
    let project = authorized_project(&mut app, &key);
    let _unpolled = app.repository_load_task(PathBuf::from("/unaccessed"), Some(project), false);
    assert!(matches!(
        app.github_availability.get(&project),
        Some(GitHubAvailability::Loading { .. })
    ));
    assert_eq!(
        confirmed_repository(&app, project),
        Some(&key),
        "a poll starting is liveness and must not withdraw authorization"
    );

    // A completion the generation fence rejects reports nothing about this
    // repository, so it may neither take authority away nor hand it back.
    let stale = app.repository_generations[&project].wrapping_sub(1);
    let _ = app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: stale,
        result: Err("superseded failure".into()),
    });
    assert_eq!(confirmed_repository(&app, project), Some(&key));

    // The identical payload at the live generation is a completed refresh that
    // reports failure, and that does revoke.
    let live = app.repository_generations[&project];
    let _ = app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: live,
        result: Err("refresh failed".into()),
    });
    assert_eq!(confirmed_repository(&app, project), None);
}

#[test]
fn a_refresh_confirming_the_same_repository_leaves_an_open_draft_submittable() {
    let mut app = tests::context_inspector_test_app(false);
    let key = github_key();
    let project = authorized_project(&mut app, &key);
    open_draft(&mut app, project);
    let _unpolled = app.repository_load_task(PathBuf::from("/unaccessed"), Some(project), false);
    app.github_availability.insert(
        project,
        GitHubAvailability::Ready {
            repository_key: key,
            pull_requests: Vec::new(),
            refreshed_at: 2,
        },
    );
    // Not polled, so no gh process runs: taking the dialog is the whole
    // observable difference between an accepted and a rejected submission.
    let _unpolled = app.update(Message::SubmitPullRequest);
    assert!(
        app.pull_request_dialog.is_none(),
        "a refresh confirming the same repository invalidated an authored draft"
    );

    // A different repository is a different authorization, and rejects.
    let project = authorized_project(&mut app, &github_key());
    open_draft(&mut app, project);
    app.github_availability.insert(
        project,
        GitHubAvailability::Ready {
            repository_key: GitHubRepoKey::new("github.com", None, "owner", "fork").unwrap(),
            pull_requests: Vec::new(),
            refreshed_at: 3,
        },
    );
    let _ = app.update(Message::SubmitPullRequest);
    assert!(
        app.pull_request_dialog.is_some(),
        "a draft authored against another repository was submitted"
    );
}

#[test]
fn pending_spawn_close_does_not_remove_card_or_reject_future_completion() {
    let mut app = tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    let card = app.workspace.focused().unwrap();
    app.cards.insert(card, CardRuntime::Loading);
    app.begin_terminal_start(card);
    for message in [Message::CloseCard(card), Message::CommitClose(card)] {
        let _ = app.update(message);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(app.workspace.order().any(|id| id == card));
        assert!(app.terminal_close_attempts.is_empty());
    }
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    let prior_terminal = session.id;
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    app.cards
        .insert(card, CardRuntime::Failed("previous terminal exited".into()));
    drop(app.finish_terminal_start(card));
    let _unpolled_spawn = app.update(Message::RetryCard(card));
    let CardKind::Terminal(replacement) = &app.card_state(card).unwrap().kind else {
        unreachable!()
    };
    assert_ne!(replacement.id, prior_terminal);
    assert!(matches!(
        replacement.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(_),
            ..
        }
    ));
    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::CommitClose(card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
    assert!(app.terminal_close_attempts.is_empty());
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close.is_some());
    assert_eq!(app.terminal_close_attempts.len(), 1);
}

#[test]
fn an_unrelated_terminal_start_does_not_hold_a_loading_card_open() {
    let mut app = tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    app.cards.insert(card, CardRuntime::Loading);
    app.begin_terminal_start(CardId::new());
    assert!(!app.pending_terminal_spawn(card));
}

/// The fence exists so one intent cannot submit two concurrent requests, and
/// so nothing automatic retries a request whose fate is unknown. It is not a
/// permanent refusal: a card whose close went unanswered used to be
/// unclosable for the rest of the process, answering every later Close with
/// "A close was already attempted" beside a Close button that could not be
/// pressed.
#[test]
fn an_unanswered_close_can_be_asked_again_once_its_report_is_dismissed() {
    let (mut app, card, terminal, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    assert_eq!(app.terminal_close_attempts.len(), 1);
    report_close(
        &mut app,
        terminal_close::CloseOutcome::Unreachable("daemon unreachable".into()),
    );

    let intent = app.terminal_close.as_ref().expect("the report is retained");
    assert!(!intent.pending);
    assert!(
        intent.detail.is_some(),
        "an unreachable owner leaves an outcome to report"
    );
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.terminal_close_dialog().is_some());

    // A report is not a question: confirming it must do nothing at all.
    let _ = app.update(Message::ConfirmTerminalClose);
    assert!(app.terminal_close.as_ref().unwrap().detail.is_some());
    assert!(app.card_state(card).unwrap().existence.is_open());

    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.terminal_close.is_none());
    assert!(
        app.terminal_close_attempts.is_empty(),
        "dismissing the report ends the attempt it reported on"
    );

    let _ = app.update(Message::CloseCard(card));
    assert!(
        app.terminal_close
            .as_ref()
            .is_some_and(|intent| intent.pending && intent.terminal == terminal),
        "the next explicit close must actually reach the owner, not a refusal"
    );
}

#[test]
fn terminal_confirmation_owns_tab_navigation_and_restores_shell_setting() {
    use super::dialog_ui::tests::{focus_after_key, key_press, shift_tab};
    let (mut app, card, _, _) = live_app();
    app.core.set_keyboard_nav(true);
    request_active_close(&mut app, card);
    assert!(!app.core.keyboard_nav());
    // Tab and Shift+Tab cycle "Don't ask again", Cancel and Close, wrapping
    // inside the dialog instead of reaching the window behind it.
    let dont_ask = super::dialog_ui::dont_ask_again_id(settings::Confirmation::CloseActiveTerminal);
    let cancel = widget::Id::new("dialog.terminal-close.cancel");
    let close = widget::Id::new("dialog.terminal-close.confirm");
    let tab = key_press(keyboard::key::Named::Tab);
    for (from, event, to) in [
        (&close, tab.clone(), &dont_ask),
        (&dont_ask, tab.clone(), &cancel),
        (&cancel, tab, &close),
        (&dont_ask, shift_tab(), &close),
        (&close, key_press(keyboard::key::Named::ArrowLeft), &cancel),
    ] {
        let (focused, captured) =
            focus_after_key(app.terminal_close_dialog().unwrap(), from.clone(), event);
        assert_eq!(focused.as_ref(), Some(to));
        assert!(captured, "the dialog keeps its focus keys from the shell");
    }
    // Space presses the focused control as Enter does: it ticks the
    // checkbox and answers with the focused button.
    let space = super::dialog_ui::tests::space();
    for (focused, expected) in [
        (
            &dont_ask,
            Message::DontAskAgain(settings::Confirmation::CloseActiveTerminal, true),
        ),
        (&cancel, Message::CancelTerminalClose),
        (&close, Message::ConfirmTerminalClose),
    ] {
        let (messages, captured) = super::dialog_ui::tests::key_control(
            app.terminal_close_dialog().unwrap(),
            focused.clone(),
            space.clone(),
        );
        assert_eq!(
            format!("{messages:?}"),
            format!("{:?}", [expected]),
            "Space presses {focused:?}"
        );
        assert!(captured);
    }
    // Someone still typing hasn't read the question: a letter leaves every
    // choice unfocused, so the Enter ending their line answers nothing.
    let keyboard::Event::KeyPressed {
        physical_key,
        location,
        modifiers,
        ..
    } = key_press(keyboard::key::Named::Enter)
    else {
        unreachable!()
    };
    let letter = keyboard::Event::KeyPressed {
        key: keyboard::Key::Character("l".into()),
        modified_key: keyboard::Key::Character("l".into()),
        physical_key,
        location,
        modifiers,
        text: Some("l".into()),
        repeat: false,
    };
    let (focused, captured) =
        focus_after_key(app.terminal_close_dialog().unwrap(), close.clone(), letter);
    assert_eq!(focused, None);
    assert!(captured);
    // A held Enter or Space answers nothing.
    for held in [key_press(keyboard::key::Named::Enter), space] {
        let keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            modifiers,
            text,
            ..
        } = held
        else {
            unreachable!()
        };
        let repeated = keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            modifiers,
            text,
            repeat: true,
        };
        let (messages, captured) = super::dialog_ui::tests::key_control(
            app.terminal_close_dialog().unwrap(),
            close.clone(),
            repeated,
        );
        assert!(messages.is_empty(), "a held key answers nothing");
        assert!(captured);
    }
    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.core.keyboard_nav());
}

#[test]
fn idle_inspection_closes_without_confirmation_and_stale_results_cannot_close_replacement() {
    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    assert!(app.terminal_close_dialog().is_none());
    report_close(&mut app, terminal_close::CloseOutcome::Closed);
    assert!(app.terminal_close.is_none());
    assert!(app.closed_terminal_cards.contains(&card));

    let (mut app, card, _, _) = live_app();
    let _ = app.update(Message::CloseCard(card));
    let old = app.terminal_close.as_ref().unwrap().clone();
    report_close(&mut app, terminal_close::CloseOutcome::Active);
    let _ = app.update(Message::CancelTerminalClose);
    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal: old.terminal,
        proof: old.proof,
        generation: old.generation,
        outcome: terminal_close::CloseOutcome::Closed,
    });
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.closed_terminal_cards.contains(&card));
    assert!(app.terminal_close.as_ref().unwrap().pending);
}

#[test]
fn terminal_close_opt_out_persists_only_on_close_and_can_be_reenabled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settings.toml");
    let (mut app, card, _, _) = live_app();
    app.settings_path = Some(path.clone());
    app.settings_write_blocked = false;
    request_active_close(&mut app, card);
    let _ = app.update(Message::DontAskAgain(
        settings::Confirmation::CloseActiveTerminal,
        true,
    ));
    let _ = app.update(Message::CancelTerminalClose);
    assert!(app.settings.confirm_terminal_close);
    assert!(!path.exists());
    request_active_close(&mut app, card);
    let _ = app.update(Message::DontAskAgain(
        settings::Confirmation::CloseActiveTerminal,
        true,
    ));
    // The process may finish while its confirmation is open. Keep this
    // preference test independent of any real daemon or process shutdown.
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    // Drive only the preference writer; the exact owner receipt is supplied
    // below, without executing a real daemon close from this unit fixture.
    drop(app.update(Message::ConfirmTerminalClose));
    let save = app.save_settings_task(false);
    tests::settle_settings_task(&mut app, save);
    assert!(!settings::load(&path).unwrap().confirm_terminal_close);
    report_close(&mut app, terminal_close::CloseOutcome::Closed);
    assert!(app.terminal_close.is_none());
    let enable = app.update(Message::SettingsChanged {
        field: SettingsField::Confirm(settings::Confirmation::CloseActiveTerminal),
        value: "true".into(),
    });
    tests::settle_settings_task(&mut app, enable);
    assert!(settings::load(&path).unwrap().confirm_terminal_close);
}

#[test]
fn enter_on_a_focused_button_leaves_the_next_click_to_the_control_under_the_pointer() {
    use iced::{
        Point, Size,
        advanced::{clipboard, widget::operation},
        keyboard, mouse,
        runtime::user_interface::{Cache, UserInterface},
    };

    // The Enter-activated button comes first: a row offers each event to its
    // children in order, so a latched button would take the click's release
    // before the button under the pointer sees it.
    let keyboard_id = widget::Id::new("enter.contract.keyboard");
    let pointer_id = widget::Id::new("enter.contract.pointer");
    let element: Element<'static, u8> = widget::row([
        widget::button::custom(design::caption("Keyboard action"))
            .id(keyboard_id.clone())
            .width(Length::Fixed(100.0))
            .height(Length::Fixed(40.0))
            .on_press(1)
            .into(),
        widget::button::custom(design::caption("Pointer action"))
            .id(pointer_id)
            .width(Length::Fixed(100.0))
            .height(Length::Fixed(40.0))
            .on_press(2)
            .into(),
    ])
    .spacing(20)
    .into();
    let mut renderer = tests::headless_renderer();
    let mut ui = UserInterface::build(
        element,
        Size::new(400.0, 100.0),
        Cache::default(),
        &mut renderer,
    );
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(keyboard_id),
    );
    let key = keyboard::Key::Named(keyboard::key::Named::Enter);
    let enter = iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    });
    let mut messages = Vec::new();
    ui.update(
        &[enter],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(messages, vec![1], "Enter activates the focused button");

    let pointer = Point::new(170.0, 20.0);
    ui.update(
        &[
            iced::Event::Mouse(mouse::Event::CursorMoved { position: pointer }),
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        mouse::Cursor::Available(pointer),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(
        messages,
        vec![1, 2],
        "the first click after Enter reaches the button under the pointer"
    );
}
