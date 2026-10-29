#![cfg(feature = "ui")]

//! Production-widget and UserInterface contracts. These create no native
//! window, accessibility client or provider; native acceptance is separate.

use cagents::ui_controls::field;
use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, renderer::Headless, widget::Tree};
use cosmic::iced::{Event, Font, Pixels, Rectangle, Size, Vector, event, keyboard, mouse};
use cosmic::{Element, Renderer, widget};
use iced_accessibility::{
    A11yNode, A11yTree, Id,
    accesskit::{Action, ActionRequest, NodeId, Role, TreeId},
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Message {
    Selected(usize),
    Sibling(usize),
    SurfaceClosed,
    Typed(String),
}

fn view(
    owner: &str,
    source: &str,
    options: Vec<String>,
    selected: Option<usize>,
    top: f32,
) -> Element<'static, Message> {
    widget::column::with_children(vec![
        cosmic::iced::widget::Space::new().height(top).into(),
        field::select(
            "Settings category",
            format!("test.{owner}.category"),
            source.to_owned(),
            options,
            selected,
            Message::Selected,
        )
        .width(200)
        .into(),
        field::select(
            "Terminal color theme",
            format!("test.{owner}.theme"),
            "terminal-theme-values",
            vec!["Automatic", "Light"],
            Some(0),
            Message::Sibling,
        )
        .width(200)
        .into(),
    ])
    .into()
}

struct Fixture {
    element: Element<'static, Message>,
    tree: Tree,
    node: layout::Node,
    renderer: Renderer,
    viewport: Rectangle,
}
impl Fixture {
    fn new(options: Vec<String>, selected: Option<usize>) -> Self {
        Self::from_element(view("owner-a", "source-a", options, selected, 0.0))
    }
    fn from_element(mut element: Element<'static, Message>) -> Self {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::DEFAULT,
                Pixels::from(16),
                Some("tiny-skia"),
            ))
            .expect("headless renderer");
        let mut tree = Tree::new(element.as_widget());
        tree.diff(element.as_widget_mut());
        let viewport = Rectangle::with_size(Size::new(360.0, 220.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        Self {
            element,
            tree,
            node,
            renderer,
            viewport,
        }
    }
    fn replace(
        &mut self,
        owner: &str,
        source: &str,
        options: Vec<String>,
        selected: Option<usize>,
        top: f32,
    ) {
        self.element = view(owner, source, options, selected, top);
        self.tree.diff(self.element.as_widget_mut());
        self.node = self.element.as_widget_mut().layout(
            &mut self.tree,
            &self.renderer,
            &layout::Limits::new(Size::ZERO, self.viewport.size()),
        );
    }
    fn export(&mut self) -> A11yTree {
        if let Some(mut overlay) = self.element.as_widget_mut().overlay(
            &mut self.tree,
            Layout::new(&self.node),
            &self.renderer,
            &self.viewport,
            Vector::ZERO,
        ) {
            overlay
                .as_overlay_mut()
                .layout(&self.renderer, self.viewport.size());
        }
        self.element.as_widget().a11y_nodes(
            Layout::new(&self.node),
            &self.tree,
            mouse::Cursor::Unavailable,
        )
    }
    fn event(&mut self, event: Event, cursor: mouse::Cursor) -> (Vec<Message>, event::Status) {
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        if let Some(mut overlay) = self.element.as_widget_mut().overlay(
            &mut self.tree,
            Layout::new(&self.node),
            &self.renderer,
            &self.viewport,
            Vector::ZERO,
        ) {
            let node = overlay
                .as_overlay_mut()
                .layout(&self.renderer, self.viewport.size());
            overlay.as_overlay_mut().update(
                &event,
                Layout::new(&node),
                cursor,
                &self.renderer,
                &mut clipboard::Null,
                &mut shell,
            );
        }
        if !shell.is_event_captured() {
            self.element.as_widget_mut().update(
                &mut self.tree,
                &event,
                Layout::new(&self.node),
                cursor,
                &self.renderer,
                &mut clipboard::Null,
                &mut shell,
                &self.viewport,
            );
        }
        let status = shell.event_status();
        drop(shell);
        (messages, status)
    }
    fn action(&mut self, target: NodeId, action: Action) -> Vec<Message> {
        let request = ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: target,
            data: None,
        };
        // Use the production preflight and real focus operation. The native
        // UserInterface applies it at this event's delivery boundary.
        let tree = self.export();
        if let Some(id) =
            cosmic::iced::runtime::user_interface::accessibility_focus_target(&tree, &request)
        {
            self.element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(id),
            );
        }
        self.event(
            Event::A11y(Id::from(target.0), request),
            mouse::Cursor::Unavailable,
        )
        .0
    }

    fn key(&mut self, key: keyboard::key::Named) -> Vec<Message> {
        self.event(
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modified_key: keyboard::Key::Named(key),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            mouse::Cursor::Unavailable,
        )
        .0
    }
    fn focused(&mut self) -> Vec<bool> {
        use cosmic::iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Focus(Vec<bool>);
        impl Operation for Focus {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _: Option<&cosmic::iced::advanced::widget::Id>,
                _: Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                self.0.push(state.is_focused());
            }
        }
        let mut focus = Focus::default();
        self.element.as_widget_mut().operate(
            &mut self.tree,
            Layout::new(&self.node),
            &self.renderer,
            &mut focus,
        );
        focus.0
    }
    #[cfg(target_os = "linux")]
    fn popup_provider(
        &self,
        options: Vec<String>,
        selected: Option<usize>,
    ) -> Box<dyn Fn() -> Element<'static, Message>> {
        let state = self.tree.children[1]
            .state
            .downcast_ref::<widget::dropdown::State>()
            .clone();
        Box::new(move || {
            widget::dropdown::menu_widget(
                Rectangle::with_size(Size::new(200.0, 32.0)),
                &state,
                4.0,
                8.0.into(),
                14.0,
                options.clone().into(),
                Vec::new().into(),
                selected,
                std::sync::Arc::new(Message::Selected),
                Some(Message::SurfaceClosed),
            )
        })
    }
}
fn nodes(tree: &A11yTree) -> impl Iterator<Item = &A11yNode> {
    tree.root().iter().chain(tree.children())
}
fn named(tree: &A11yTree, name: &str) -> A11yNode {
    nodes(tree)
        .find(|node| node.node().label() == Some(name))
        .unwrap_or_else(|| panic!("missing actual node {name}"))
        .clone()
}
fn target(node: &A11yNode) -> NodeId {
    node.id().clone().into()
}
fn options(tree: &A11yTree) -> Vec<A11yNode> {
    let children = nodes(tree)
        .filter(|node| node.node().role() == Role::ListBox)
        .flat_map(|node| node.node().children().iter().copied())
        .collect::<Vec<_>>();
    nodes(tree)
        .filter(|node| children.contains(&target(node)))
        .cloned()
        .collect()
}
fn displayed_selection(tree: &A11yTree, name: &str) -> Option<A11yNode> {
    let root = named(tree, name);
    nodes(tree)
        .find(|node| {
            root.node().children().contains(&target(node))
                && node.node().role() == Role::ListBoxOption
        })
        .cloned()
}
fn choices(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("Option {index}")).collect()
}

#[test]
fn dropdown_accessibility_names_values_and_absent_selection_are_real_control_semantics() {
    for selected in [None, Some(99), Some(1)] {
        let mut fixture = Fixture::new(vec!["Automatic".into(), "Terminal".into()], selected);
        let tree = fixture.export();
        let root = named(&tree, "Settings category");
        let sibling = named(&tree, "Terminal color theme");
        assert_eq!(root.node().role(), Role::ComboBox);
        assert_eq!(
            root.node().value(),
            (selected == Some(1)).then_some("Terminal")
        );
        assert_eq!(sibling.node().value(), Some("Automatic"));
        let displayed = displayed_selection(&tree, "Settings category");
        assert_eq!(displayed.is_some(), selected == Some(1));
        if let Some(displayed) = displayed {
            assert_eq!(displayed.node().label(), Some("Terminal"));
            assert_eq!(displayed.node().is_selected(), Some(true));
            assert!(displayed.node().is_read_only());
            assert!(!displayed.node().supports_action(Action::Click));
            assert!(!displayed.node().supports_action(Action::Focus));
            let bounds = displayed.node().bounds().unwrap();
            let control = root.node().bounds().unwrap();
            assert!(bounds.x0 >= control.x0 && bounds.x1 <= control.x1);
            assert!(bounds.y0 >= control.y0 && bounds.y1 <= control.y1);
        }
        assert_ne!(target(&root), target(&sibling));
        assert_eq!(root.node().is_expanded(), Some(false));
        assert!(root.node().supports_action(Action::Focus));
        assert!(root.node().supports_action(Action::Expand));
        assert!(options(&tree).is_empty());
    }
    let mut empty = Fixture::new(Vec::new(), None);
    let root = named(&empty.export(), "Settings category");
    assert!(root.node().is_disabled());
    assert!(!root.node().supports_action(Action::Click));
    assert!(empty.action(target(&root), Action::Expand).is_empty());
    assert_eq!(
        named(&empty.export(), "Settings category")
            .node()
            .is_expanded(),
        Some(false)
    );
}

#[test]
fn dropdown_accessibility_focus_expand_select_and_repeat_use_current_emitted_targets() {
    let mut fixture = Fixture::new(vec!["Automatic".into(), "Terminal".into()], Some(0));
    let root = target(&named(&fixture.export(), "Settings category"));
    assert!(fixture.action(root, Action::Focus).is_empty());
    assert_eq!(
        fixture.focused(),
        [true, false],
        "Focus reaches the real dropdown operation state"
    );
    assert!(fixture.action(root, Action::Expand).is_empty());
    let first = fixture.export();
    let options_before = options(&first);
    assert_eq!(options_before.len(), 2);
    assert_eq!(options_before[0].node().is_selected(), Some(true));
    assert_eq!(options_before[1].node().is_selected(), Some(false));
    assert!(!options_before[1].node().supports_action(Action::Focus));
    assert_eq!(
        named(&first, "Settings category").node().is_expanded(),
        Some(true)
    );
    assert!(fixture.action(root, Action::Expand).is_empty());
    assert_eq!(
        options(&fixture.export())
            .iter()
            .map(target)
            .collect::<Vec<_>>(),
        options_before.iter().map(target).collect::<Vec<_>>(),
        "repeated Expand must not create a new opening"
    );
    assert_eq!(
        fixture.action(target(&options_before[1]), Action::Click),
        [Message::Selected(1)]
    );
    assert_eq!(
        fixture.focused(),
        [true, false],
        "option Click keeps focus on its owning control"
    );
    assert!(
        fixture
            .action(target(&options_before[1]), Action::Click)
            .is_empty(),
        "queued duplicate cannot select twice"
    );
    assert!(options(&fixture.export()).is_empty());
    assert_eq!(
        named(&fixture.export(), "Terminal color theme")
            .node()
            .value(),
        Some("Automatic")
    );
    assert!(fixture.action(root, Action::Collapse).is_empty());
    assert!(fixture.action(root, Action::Collapse).is_empty());
    assert!(fixture.action(root, Action::Expand).is_empty());
    assert_ne!(
        target(&options(&fixture.export())[1]),
        target(&options_before[1]),
        "reopening retires old option IDs"
    );
    assert!(
        fixture
            .action(target(&options_before[0]), Action::Click)
            .is_empty()
    );
}

#[test]
fn dropdown_accessibility_rebuild_source_reorder_and_owner_replacement_have_distinct_custody() {
    let labels = vec!["Same".into(), "Same".into()];
    let mut fixture = Fixture::new(labels.clone(), Some(0));
    let root = target(&named(&fixture.export(), "Settings category"));
    let displayed = target(&displayed_selection(&fixture.export(), "Settings category").unwrap());
    fixture.action(root, Action::Expand);
    let first = options(&fixture.export());
    assert_ne!(
        target(&first[0]),
        target(&first[1]),
        "duplicate labels are distinct choices"
    );
    fixture.replace("owner-a", "source-a", labels.clone(), Some(0), 0.0);
    assert_eq!(target(&named(&fixture.export(), "Settings category")), root);
    assert_eq!(
        target(&displayed_selection(&fixture.export(), "Settings category").unwrap()),
        displayed
    );
    assert_eq!(
        options(&fixture.export())
            .iter()
            .map(target)
            .collect::<Vec<_>>(),
        first.iter().map(target).collect::<Vec<_>>(),
        "unrelated rebuild preserves active opening"
    );
    // Identical labels now refer to different underlying choices.
    fixture.replace("owner-a", "source-b", labels.clone(), Some(0), 0.0);
    assert_ne!(
        target(&displayed_selection(&fixture.export(), "Settings category").unwrap()),
        displayed,
        "a replacement source gets a fresh selected display identity"
    );
    fixture.action(root, Action::Expand);
    assert!(fixture.action(target(&first[1]), Action::Click).is_empty());
    let next = options(&fixture.export());
    assert_eq!(next.len(), 2);
    assert_ne!(target(&next[1]), target(&first[1]));
    fixture.replace(
        "owner-a",
        "reordered-choice-ids",
        labels.clone(),
        Some(0),
        0.0,
    );
    assert!(fixture.action(target(&next[0]), Action::Click).is_empty());
    fixture.action(root, Action::Expand);
    let reordered = options(&fixture.export());
    assert_eq!(
        fixture.action(target(&reordered[1]), Action::Click),
        [Message::Selected(1)]
    );
    fixture.replace("owner-b", "reordered-choice-ids", labels, Some(0), 0.0);
    let replaced_root = target(&named(&fixture.export(), "Settings category"));
    assert_ne!(replaced_root, root);
    assert!(fixture.action(root, Action::Expand).is_empty());
    assert!(options(&fixture.export()).is_empty());
    fixture.action(replaced_root, Action::Expand);
    assert_eq!(options(&fixture.export()).len(), 2);
}

#[test]
fn dropdown_accessibility_real_scrolled_upward_bounds_pointer_and_keyboard_share_choices() {
    let mut fixture =
        Fixture::from_element(view("owner-a", "source-a", choices(80), Some(0), 150.0));
    let root = named(&fixture.export(), "Settings category");
    let bounds = root.node().bounds().unwrap();
    fixture.action(target(&root), Action::Expand);
    let tree = fixture.export();
    let menu = named(&tree, "Settings category options");
    assert!(
        menu.node().bounds().unwrap().y1 <= bounds.y0,
        "menu really opens above its trigger"
    );
    assert_eq!(options(&tree).len(), 80);
    let viewport = menu.node().bounds().unwrap();
    let before_wheel = options(&tree)[0].node().bounds().unwrap().y0;
    fixture.event(
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: -5.0 },
        }),
        mouse::Cursor::Available(cosmic::iced::Point::new(
            ((viewport.x0 + viewport.x1) / 2.0) as f32,
            ((viewport.y0 + viewport.y1) / 2.0) as f32,
        )),
    );
    let after_wheel = fixture.export();
    assert_eq!(
        options(&after_wheel).iter().map(target).collect::<Vec<_>>(),
        options(&tree).iter().map(target).collect::<Vec<_>>()
    );
    assert!(
        options(&after_wheel)[0].node().bounds().unwrap().y0 < before_wheel,
        "native option bounds must follow actual pointer-wheel scrolling"
    );
    fixture.key(keyboard::key::Named::End);
    let tree = fixture.export();
    let last = options(&tree).pop().unwrap();
    let viewport = named(&tree, "Settings category options")
        .node()
        .bounds()
        .unwrap();
    let last_bounds = last.node().bounds().unwrap();
    assert!(
        last_bounds.y0 >= viewport.y0 - 1.0 && last_bounds.y1 <= viewport.y1 + 1.0,
        "export follows actual revealed scroll translation"
    );
    assert_eq!(
        fixture.action(target(&last), Action::Click),
        [Message::Selected(79)]
    );
    fixture.action(target(&root), Action::Expand);
    fixture.key(keyboard::key::Named::Home);
    let first = options(&fixture.export())[0].clone();
    let bounds = first.node().bounds().unwrap();
    let position = cosmic::iced::Point::new(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    let messages = fixture
        .event(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(cosmic::iced::Point::new(
                position.x as f32,
                position.y as f32,
            )),
        )
        .0;
    assert_eq!(messages, [Message::Selected(0)]);
    fixture.action(target(&root), Action::Expand);
    assert!(fixture.key(keyboard::key::Named::Escape).is_empty());
    assert!(options(&fixture.export()).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn dropdown_accessibility_materialized_popup_rejects_second_action_and_removed_control_owner() {
    let labels = choices(3);
    let mut owner = Fixture::new(labels.clone(), Some(0));
    let root = target(&named(&owner.export(), "Settings category"));
    owner.action(root, Action::Expand);
    let provider = owner.popup_provider(labels.clone(), Some(0));
    let mut popup = Fixture::from_element(provider());
    let old = options(&popup.export());
    assert_eq!(old.len(), 3);
    assert_eq!(
        popup.action(target(&old[2]), Action::Click),
        [Message::Selected(2), Message::SurfaceClosed]
    );
    assert!(
        !popup
            .action(target(&old[2]), Action::Click)
            .iter()
            .any(|message| matches!(message, Message::Selected(_))),
        "materialized overlay has already spent its selection authorization"
    );
    owner.action(root, Action::Expand);
    let provider = owner.popup_provider(labels.clone(), Some(0));
    let mut popup = Fixture::from_element(provider());
    let old = options(&popup.export());
    owner.replace("owner-b", "source-a", labels, Some(0), 0.0);
    assert!(
        !popup
            .action(target(&old[1]), Action::Click)
            .iter()
            .any(|message| matches!(message, Message::Selected(_))),
        "a removed owning Tree retires an already materialized popup"
    );
    let mut rebuilt_old_popup = Fixture::from_element(provider());
    assert!(options(&rebuilt_old_popup.export()).is_empty());
    assert!(
        !rebuilt_old_popup
            .action(target(&old[1]), Action::Click)
            .iter()
            .any(|message| matches!(message, Message::Selected(_)))
    );
}

fn assert_active_targets_exist(tree: &A11yTree) {
    let emitted = nodes(tree).map(target).collect::<Vec<_>>();
    for node in nodes(tree) {
        if let Some(active) = node.node().active_descendant() {
            assert!(
                emitted.contains(&active),
                "active descendant must belong to this exported tree"
            );
            if node.node().role() == Role::ComboBox {
                assert!(
                    nodes(tree).any(|popup| node.node().controls().contains(&target(popup))
                        && popup.node().children().contains(&active)),
                    "a sibling active descendant must belong to the controlled popup"
                );
            }
        }
    }
}

#[test]
fn dropdown_accessibility_list_collapse_unknown_targets_and_highlight_preserve_committed_choice() {
    let mut fixture = Fixture::new(choices(3), Some(0));
    let root = target(&named(&fixture.export(), "Settings category"));
    fixture.action(root, Action::Expand);
    let tree = fixture.export();
    let current = options(&tree);
    assert!(
        fixture
            .action(target(&current[1]), Action::Expand)
            .is_empty()
    );
    let unknown = NodeId::from(iced_accessibility::A11yId::from(Id::unique()));
    assert!(fixture.action(unknown, Action::Click).is_empty());
    assert_eq!(
        options(&fixture.export())
            .iter()
            .map(target)
            .collect::<Vec<_>>(),
        current.iter().map(target).collect::<Vec<_>>()
    );
    fixture.key(keyboard::key::Named::ArrowDown);
    let tree = fixture.export();
    assert_active_targets_exist(&tree);
    assert_eq!(
        named(&tree, "Settings category").node().active_descendant(),
        Some(target(&current[1]))
    );
    assert_eq!(
        named(&tree, "Settings category options")
            .node()
            .active_descendant(),
        Some(target(&current[1]))
    );
    assert_eq!(
        options(&tree)[0].node().is_selected(),
        Some(true),
        "highlight does not commit the setting"
    );
    assert_eq!(options(&tree)[1].node().is_selected(), Some(false));
    let list = target(&named(&tree, "Settings category options"));
    assert!(fixture.action(list, Action::Collapse).is_empty());
    assert!(options(&fixture.export()).is_empty());
    assert!(fixture.action(list, Action::Collapse).is_empty());
    assert_eq!(
        named(&fixture.export(), "Settings category").node().value(),
        Some("Option 0")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dropdown_accessibility_separate_popup_active_descendants_stay_in_their_own_exported_tree() {
    let mut owner = Fixture::new(choices(3), Some(0));
    let root = target(&named(&owner.export(), "Settings category"));
    owner.action(root, Action::Expand);
    let mut popup = Fixture::from_element(owner.popup_provider(choices(3), Some(0))());
    assert_active_targets_exist(&owner.export());
    assert_active_targets_exist(&popup.export());
    let element = widget::dropdown(choices(3), Some(0), Message::Selected)
        .name("Native popup selector")
        .with_popup(
            cosmic::iced::window::Id::unique(),
            |_| Message::SurfaceClosed,
            |message| message,
        )
        .into();
    let mut separate_owner = Fixture::from_element(element);
    let root = target(&named(&separate_owner.export(), "Native popup selector"));
    separate_owner.action(root, Action::Expand);
    let tree = separate_owner.export();
    assert_active_targets_exist(&tree);
    assert!(
        options(&tree).is_empty(),
        "the parent window must not export popup-window descendants"
    );
    assert_eq!(
        named(&tree, "Native popup selector")
            .node()
            .active_descendant(),
        None
    );
}

#[test]
fn dropdown_accessibility_native_preflight_prevents_batched_and_separate_sibling_focus_resurrection()
 {
    use cosmic::iced::advanced::widget::{Operation, operation};
    use cosmic::iced::runtime::user_interface::{Cache, UserInterface};
    #[derive(Default)]
    struct Focus(Vec<(NodeId, bool)>);
    impl Operation for Focus {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&cosmic::iced::advanced::widget::Id>,
            _: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if let Some(id) = id {
                self.0.push((
                    NodeId::from(iced_accessibility::A11yId::from(id.clone())),
                    state.is_focused(),
                ));
            }
        }
    }
    fn request(target: NodeId, action: Action) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: target,
            data: None,
        }
    }
    fn event(request: ActionRequest) -> Event {
        Event::A11y(Id::from(request.target_node.0), request)
    }
    for batched in [false, true] {
        for activation in [Action::Expand, Action::Click] {
            let fixture = Fixture::new(choices(3), Some(0));
            let mut renderer = fixture.renderer;
            let content: Element<'static, Message> = widget::column::with_children(vec![
                field::text("Notes", "held")
                    .id(widget::Id::new("runtime.notes"))
                    .on_input(Message::Typed)
                    .into(),
                view("owner-a", "source-a", choices(3), Some(0), 0.0),
                field::select(
                    "Unavailable choice",
                    "runtime.empty",
                    "empty-choices",
                    Vec::<String>::new(),
                    None,
                    Message::Selected,
                )
                .into(),
            ])
            .into();
            let mut ui = UserInterface::build(
                content,
                Size::new(360.0, 300.0),
                Cache::default(),
                &mut renderer,
            );
            let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let notes = target(
                nodes(&tree)
                    .find(|node| node.node().role() == Role::TextInput)
                    .unwrap(),
            );
            let disabled = target(&named(&tree, "Unavailable choice"));
            let a = target(&named(&tree, "Settings category"));
            let b = target(&named(&tree, "Terminal color theme"));
            let mut messages = Vec::new();
            let mut queued = Vec::new();
            // The real text field locally acknowledges its queued Focus.
            // It must not regain focus beside a later dropdown activation.
            let notes_focus = request(notes, Action::Focus);
            if batched {
                queued.push(event(notes_focus));
            } else {
                ui.update(
                    &[event(notes_focus)],
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
            }
            // An accessibility client may activate each control directly,
            // without sending Focus first. Use the actual native preflight
            // for both separate events and a queued A/B action batch.
            // UserInterface applies production focus at event delivery.
            for target in [a, b] {
                let action = request(target, activation);
                if batched {
                    queued.push(event(action));
                } else {
                    ui.update(
                        &[event(action)],
                        mouse::Cursor::Unavailable,
                        &mut renderer,
                        &mut clipboard::Null,
                        &mut messages,
                    );
                }
            }
            if batched {
                ui.update(
                    &queued,
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
            }
            let mut focus = Focus::default();
            ui.operate(&renderer, &mut focus);
            assert_eq!(
                focus
                    .0
                    .iter()
                    .filter(|(_, focused)| *focused)
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                [b]
            );
            let down = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::ArrowDown),
                modified_key: keyboard::Key::Named(keyboard::key::Named::ArrowDown),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::ArrowDown),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            ui.update(
                &[down],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
            assert_active_targets_exist(&tree);
            assert_eq!(
                named(&tree, "Settings category").node().is_expanded(),
                Some(false)
            );
            assert_eq!(
                named(&tree, "Terminal color theme").node().is_expanded(),
                Some(true)
            );
            let enter = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            ui.update(
                &[enter],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages,
                [Message::Sibling(1)],
                "one Enter may commit only the currently focused menu"
            );
            let notes_focus = request(notes, Action::Focus);
            ui.update(
                &[event(notes_focus)],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            // A stale root target or unsupported option action cannot steal focus.
            let absent = NodeId::from(iced_accessibility::A11yId::from(Id::unique()));
            let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let focus_target = cosmic::iced::runtime::user_interface::accessibility_focus_target;
            assert!(focus_target(&tree, &request(absent, Action::Expand)).is_none());
            assert!(focus_target(&tree, &request(b, Action::SetValue)).is_none());
            for action in [Action::Focus, Action::Click, Action::Expand] {
                assert!(
                    focus_target(&tree, &request(disabled, action)).is_none(),
                    "disabled target must not steal focus"
                );
            }
            ui.update(
                &[
                    event(request(absent, Action::Expand)),
                    event(request(b, Action::SetValue)),
                ],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let mut focus = Focus::default();
            ui.operate(&renderer, &mut focus);
            assert_eq!(
                focus
                    .0
                    .iter()
                    .filter(|(_, focused)| *focused)
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                [notes]
            );
            let space = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(" ".into()),
                modified_key: keyboard::Key::Character(" ".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Space),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some(" ".into()),
                repeat: false,
            });
            messages.clear();
            ui.update(
                &[space],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                !messages.is_empty(),
                "the focused text input must receive Space"
            );
            assert!(
                messages
                    .iter()
                    .all(|message| matches!(message, Message::Typed(_)))
            );
            assert_eq!(
                named(
                    &ui.a11y_nodes(mouse::Cursor::Unavailable),
                    "Terminal color theme"
                )
                .node()
                .is_expanded(),
                Some(false),
                "typing in the text field cannot reopen an unfocused dropdown"
            );
        }
    }
}

#[test]
fn dropdown_accessibility_native_batches_order_root_collapse_and_overlay_choice() {
    use cosmic::iced::advanced::widget::Operation;
    use cosmic::iced::runtime::user_interface::{Cache, UserInterface};
    struct Materialize;
    impl Operation for Materialize {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
    }
    fn action(target: NodeId, action: Action) -> Event {
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
    for collapse_first in [true, false] {
        let fixture = Fixture::new(choices(3), Some(0));
        let mut renderer = fixture.renderer;
        let mut ui = UserInterface::build(
            view("ordered-owner", "ordered-source", choices(3), Some(0), 0.0),
            Size::new(360.0, 300.0),
            Cache::default(),
            &mut renderer,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let a = target(&named(&tree, "Settings category"));
        let b = target(&named(&tree, "Terminal color theme"));
        let mut messages = Vec::new();
        ui.update(
            &[action(a, Action::Expand)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        // Use the actual UI operation/layout path to materialize A's overlay.
        ui.operate(&renderer, &mut Materialize);
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let old_option = target(&options(&tree)[1]);
        let batch = if collapse_first {
            vec![
                action(a, Action::Collapse),
                action(old_option, Action::Click),
            ]
        } else {
            vec![action(old_option, Action::Click), action(b, Action::Expand)]
        };
        let (_, statuses) = ui.update(
            &batch,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses.len(), batch.len());
        assert_eq!(
            messages,
            if collapse_first {
                Vec::new()
            } else {
                vec![Message::Selected(1)]
            },
            "root and overlay requests must run in their original order"
        );
        ui.operate(&renderer, &mut Materialize);
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        assert_eq!(
            named(&tree, "Settings category").node().is_expanded(),
            Some(false)
        );
        assert_eq!(
            named(&tree, "Terminal color theme").node().is_expanded(),
            Some(!collapse_first)
        );
        if collapse_first {
            assert!(options(&tree).is_empty());
        } else {
            let current = target(&options(&tree)[1]);
            ui.update(
                &[
                    action(current, Action::Click),
                    action(current, Action::Click),
                ],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages, [Message::Selected(1), Message::Sibling(1)]);
        }
    }
}

#[test]
fn dropdown_accessibility_native_mixed_batch_keeps_press_before_later_motion() {
    use cosmic::iced::runtime::user_interface::{Cache, UserInterface};
    let fixture = Fixture::new(choices(3), Some(0));
    let mut renderer = fixture.renderer;
    let mut ui = UserInterface::build(
        view("pointer-owner", "pointer-source", choices(3), Some(0), 0.0),
        Size::new(360.0, 300.0),
        Cache::default(),
        &mut renderer,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let point = |name| {
        let bounds = named(&tree, name).node().bounds().unwrap();
        cosmic::iced::Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        )
    };
    let first = point("Settings category");
    let later = point("Terminal color theme");
    let mut messages = Vec::new();
    ui.update(
        &[Event::Mouse(mouse::Event::CursorMoved { position: first })],
        mouse::Cursor::Available(first),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let absent = NodeId::from(iced_accessibility::A11yId::from(Id::unique()));
    let batch = [
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Event::A11y(
            Id::from(absent.0),
            ActionRequest {
                action: Action::Expand,
                target_tree: TreeId::ROOT,
                target_node: absent,
                data: None,
            },
        ),
        Event::Mouse(mouse::Event::CursorMoved { position: later }),
    ];
    let (_, statuses) = ui.update(
        &batch,
        mouse::Cursor::Available(later),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert_eq!(statuses.len(), batch.len());
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    assert!(messages.is_empty());
    assert_eq!(
        named(&tree, "Settings category").node().is_expanded(),
        Some(true)
    );
    assert_eq!(
        named(&tree, "Terminal color theme").node().is_expanded(),
        Some(false)
    );
}
