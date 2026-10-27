#![cfg(feature = "ui")]

use cosmic::iced::advanced::{
    Layout, Shell, clipboard, layout,
    renderer::Headless,
    widget::{Id, Operation, Tree, operation},
};
use cosmic::iced::{Event, Font, Pixels, Rectangle, Size, Vector, event, keyboard, mouse};
use cosmic::{Element, Renderer, widget};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Message {
    Before,
    Selected(usize),
    After,
    SurfaceClosed,
}

fn renderer() -> Renderer {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::DEFAULT,
            Pixels::from(16),
            Some("tiny-skia"),
        ))
        .expect("headless renderer")
}

fn view(options: Vec<String>, selected: Option<usize>) -> Element<'static, Message> {
    view_with_typography(
        options,
        selected,
        14.0,
        None,
        cosmic::iced::widget::text::LineHeight::Relative(1.2),
    )
}

fn view_with_typography(
    options: Vec<String>,
    selected: Option<usize>,
    size: f32,
    font: Option<Font>,
    line_height: cosmic::iced::widget::text::LineHeight,
) -> Element<'static, Message> {
    let mut dropdown = widget::dropdown(options, selected, Message::Selected)
        .width(200)
        .text_size(size)
        .text_line_height(line_height);
    if let Some(font) = font {
        dropdown = dropdown.font(font);
    }
    widget::column::with_children(vec![
        widget::button::text("Before")
            .on_press(Message::Before)
            .into(),
        // Ordinary Settings callers do not supply an explicit dropdown ID.
        dropdown.into(),
        widget::button::text("After")
            .on_press(Message::After)
            .into(),
    ])
    .into()
}

fn press(key: keyboard::Key, modifiers: keyboard::Modifiers, repeat: bool) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    })
}

fn named(key: keyboard::key::Named) -> Event {
    press(
        keyboard::Key::Named(key),
        keyboard::Modifiers::empty(),
        false,
    )
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
        Self::from_element(view(options, selected))
    }

    fn from_element(mut element: Element<'static, Message>) -> Self {
        let renderer = renderer();
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

    fn replace(&mut self, options: Vec<String>, selected: Option<usize>) {
        self.replace_element(view(options, selected));
    }

    fn replace_element(&mut self, element: Element<'static, Message>) {
        self.element = element;
        self.tree.diff(self.element.as_widget_mut());
        self.node = self.element.as_widget_mut().layout(
            &mut self.tree,
            &self.renderer,
            &layout::Limits::new(Size::ZERO, self.viewport.size()),
        );
    }

    fn resize(&mut self, size: Size) {
        self.viewport = Rectangle::with_size(size);
        self.node = self.element.as_widget_mut().layout(
            &mut self.tree,
            &self.renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
    }

    fn operate(&mut self, mut operation: Box<dyn Operation>) {
        loop {
            self.element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                operation.as_mut(),
            );
            match operation.finish() {
                operation::Outcome::Chain(next) => operation = next,
                _ => break,
            }
        }
    }

    fn tab(&mut self, backwards: bool) {
        if backwards {
            self.operate(Box::new(operation::focusable::focus_previous::<()>()));
        } else {
            self.operate(Box::new(operation::focusable::focus_next::<()>()));
        }
    }

    fn focused(&mut self) -> Vec<(Id, bool)> {
        #[derive(Default)]
        struct Probe(Vec<(Id, bool)>);
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&Id>,
                _: Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                self.0.push((id.unwrap().clone(), state.is_focused()));
            }
        }
        let mut probe = Probe::default();
        self.element.as_widget_mut().operate(
            &mut self.tree,
            Layout::new(&self.node),
            &self.renderer,
            &mut probe,
        );
        probe.0
    }

    fn event(&mut self, event: Event, cursor: mouse::Cursor) -> (Vec<Message>, event::Status) {
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        // Match native dispatch: the real overlay receives each event first.
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

    fn key(&mut self, key: keyboard::key::Named) -> (Vec<Message>, event::Status) {
        self.event(named(key), mouse::Cursor::Unavailable)
    }

    fn open(&mut self) -> bool {
        self.element
            .as_widget_mut()
            .overlay(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                &self.viewport,
                Vector::ZERO,
            )
            .is_some()
    }

    fn scroll(&mut self) -> (Rectangle, f32, f32) {
        #[derive(Default)]
        struct Probe(Option<(Rectangle, f32, f32)>);
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&Id>,
                bounds: Rectangle,
                content: Rectangle,
                translation: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.0 = Some((bounds, content.height, translation.y));
            }
        }
        let mut overlay = self
            .element
            .as_widget_mut()
            .overlay(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                &self.viewport,
                Vector::ZERO,
            )
            .expect("open dropdown");
        let node = overlay
            .as_overlay_mut()
            .layout(&self.renderer, self.viewport.size());
        let mut probe = Probe::default();
        overlay
            .as_overlay_mut()
            .operate(Layout::new(&node), &self.renderer, &mut probe);
        probe.0.expect("actual native scrollable")
    }

    #[cfg(target_os = "linux")]
    fn retained_popup(
        &self,
        options: Vec<String>,
        selected: Option<usize>,
    ) -> Element<'static, Message> {
        self.retained_popup_provider(options, selected)()
    }

    #[cfg(target_os = "linux")]
    fn retained_popup_provider(
        &self,
        options: Vec<String>,
        selected: Option<usize>,
    ) -> Box<dyn Fn() -> Element<'static, Message>> {
        self.retained_popup_provider_with_size(options, selected, 14.0)
    }

    #[cfg(target_os = "linux")]
    fn retained_popup_provider_with_size(
        &self,
        options: Vec<String>,
        selected: Option<usize>,
        text_size: f32,
    ) -> Box<dyn Fn() -> Element<'static, Message>> {
        // This is the public ordinary popup widget used by the Wayland path,
        // with the same State clone and owned choices as its real retained
        // view provider. It is not a compositor/native-window execution claim.
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
                text_size,
                options.clone().into(),
                Vec::new().into(),
                selected,
                std::sync::Arc::new(Message::Selected),
                Some(Message::SurfaceClosed),
            )
        })
    }
}

fn choices(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("Option {index}")).collect()
}

#[test]
fn dropdown_keyboard_focus_commit_abort_and_tab_use_real_retained_overlay() {
    use keyboard::key::Named;
    let mut fixture = Fixture::new(choices(5), Some(1));
    fixture.tab(false);
    fixture.tab(false);
    assert_eq!(
        fixture
            .focused()
            .iter()
            .map(|(_, focused)| *focused)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    let id = fixture.focused()[1].0.clone();
    fixture.replace(choices(5), Some(1));
    assert_eq!(
        fixture.focused()[1],
        (id, true),
        "unnamed ID and focus survive reconstruction"
    );
    assert!(fixture.key(Named::Enter).0.is_empty());
    assert!(fixture.open());
    assert!(fixture.key(Named::ArrowDown).0.is_empty());
    assert_eq!(fixture.key(Named::Enter).0, [Message::Selected(2)]);
    assert!(!fixture.open());
    assert!(
        fixture
            .event(
                press(
                    keyboard::Key::Named(Named::Enter),
                    keyboard::Modifiers::empty(),
                    true
                ),
                mouse::Cursor::Unavailable
            )
            .0
            .is_empty()
    );
    assert!(!fixture.open(), "held commit cannot reopen the menu");
    fixture.key(Named::Enter);
    fixture.key(Named::End);
    assert_eq!(
        fixture.key(Named::Escape),
        (Vec::new(), event::Status::Captured)
    );
    assert!(!fixture.open());
    fixture.event(
        press(
            keyboard::Key::Character(" ".into()),
            keyboard::Modifiers::empty(),
            false,
        ),
        mouse::Cursor::Unavailable,
    );
    assert_eq!(
        fixture.event(
            press(
                keyboard::Key::Named(Named::Tab),
                keyboard::Modifiers::SHIFT,
                false
            ),
            mouse::Cursor::Unavailable
        ),
        (Vec::new(), event::Status::Ignored)
    );
    assert!(!fixture.open());
    fixture.tab(true); // the existing native keyboard navigator runs once
    assert_eq!(
        fixture
            .focused()
            .iter()
            .map(|(_, focused)| *focused)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
    fixture.tab(false);
    fixture.event(
        press(
            keyboard::Key::Character(" ".into()),
            keyboard::Modifiers::empty(),
            false,
        ),
        mouse::Cursor::Unavailable,
    );
    assert!(fixture.open(), "Linux Character-space opens");
    assert_eq!(
        fixture.event(
            Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Character(" ".into()),
                modified_key: keyboard::Key::Character(" ".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Space),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }),
            mouse::Cursor::Unavailable
        ),
        (Vec::new(), event::Status::Captured)
    );
    assert_eq!(
        fixture.event(
            press(
                keyboard::Key::Named(Named::Enter),
                keyboard::Modifiers::CTRL,
                false
            ),
            mouse::Cursor::Unavailable
        ),
        (Vec::new(), event::Status::Ignored)
    );

    fixture.key(Named::Home);
    assert_eq!(fixture.key(Named::Enter).0, [Message::Selected(0)]);
    fixture.key(Named::Enter);
    assert_eq!(fixture.key(Named::Tab).1, event::Status::Ignored);
    fixture.tab(false);
    assert_eq!(
        fixture
            .focused()
            .iter()
            .map(|(_, focused)| *focused)
            .collect::<Vec<_>>(),
        [false, false, true]
    );
}

#[test]
fn dropdown_long_menu_reveals_keyboard_row_without_resetting_pointer_scroll() {
    use keyboard::key::Named;
    let mut fixture = Fixture::new(choices(100), Some(0));
    fixture.tab(false);
    fixture.tab(false);
    fixture.key(Named::Enter);
    let (bounds, content, offset) = fixture.scroll();
    assert_eq!(offset, 0.0);
    assert!(content > bounds.height * 5.0);
    let cursor = mouse::Cursor::Available(bounds.center());
    fixture.event(
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: -4.0 },
        }),
        cursor,
    );
    let (_, _, scrolled) = fixture.scroll();
    assert!(
        scrolled > 0.0,
        "pointer wheel changes the real scroll offset"
    );
    assert_eq!(
        fixture.scroll().2,
        scrolled,
        "successive native overlay layouts retain pointer browsing"
    );
    fixture.resize(Size::new(300.0, 180.0));
    assert_eq!(
        fixture.scroll().2,
        scrolled,
        "resize preserves pointer wheel browsing"
    );
    fixture.resize(Size::new(360.0, 220.0));
    fixture.key(Named::End);
    let (bounds, content, offset) = fixture.scroll();
    assert!(
        offset >= content - bounds.height - 20.0,
        "last keyboard row revealed"
    );
    fixture.resize(Size::new(300.0, 145.0));
    let (short_bounds, short_content, short_offset) = fixture.scroll();
    assert!(
        short_bounds.height < bounds.height,
        "actual overlay viewport shrinks"
    );
    assert!(
        short_offset >= short_content - short_bounds.height - 20.0,
        "resizing keeps the keyboard-highlighted last row visible before Enter"
    );
    assert_eq!(
        fixture.scroll().2,
        short_offset,
        "unchanged layouts retain the revealed offset"
    );
    fixture.key(Named::ArrowUp);
    assert_eq!(fixture.key(Named::Enter).0, [Message::Selected(98)]);
    fixture.key(Named::Enter);
    fixture.key(Named::End);
    fixture.key(Named::Home);
    assert!(
        fixture.scroll().2 < 20.0,
        "Home reveals the first row again"
    );
}

#[test]
fn dropdown_empty_and_pointer_focus_handover_do_not_leave_keyboard_ownership() {
    use keyboard::key::Named;
    let mut empty = Fixture::new(Vec::new(), None);
    empty.tab(false);
    empty.tab(false);
    assert_eq!(
        empty.focused().len(),
        2,
        "empty dropdown is not a focus trap"
    );
    assert!(!empty.open());
    let mut fixture = Fixture::new(choices(2), Some(0));
    let trigger = Layout::new(&fixture.node)
        .children()
        .nth(1)
        .unwrap()
        .bounds()
        .center();
    fixture.event(
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        mouse::Cursor::Available(trigger),
    );
    fixture.key(Named::Escape);
    let after = Layout::new(&fixture.node)
        .children()
        .nth(2)
        .unwrap()
        .bounds()
        .center();
    fixture.event(
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        mouse::Cursor::Available(after),
    );
    fixture.event(
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        mouse::Cursor::Available(after),
    );
    assert!(
        !fixture.focused()[1].1,
        "outside pointer press clears dropdown focus"
    );
    assert!(!fixture.key(Named::Enter).0.contains(&Message::Selected(0)));
    assert!(!fixture.open());
}

#[cfg(target_os = "linux")]
#[test]
fn dropdown_retained_popup_rejects_replaced_source_and_old_open_phase() {
    use keyboard::key::Named;
    fn update(
        popup: &mut Element<'static, Message>,
        tree: &mut Tree,
        renderer: &Renderer,
    ) -> Vec<Message> {
        update_event(
            popup,
            tree,
            renderer,
            named(Named::Enter),
            mouse::Cursor::Unavailable,
        )
    }
    fn update_event(
        popup: &mut Element<'static, Message>,
        tree: &mut Tree,
        renderer: &Renderer,
        event: Event,
        cursor: mouse::Cursor,
    ) -> Vec<Message> {
        let node = popup.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 200.0)),
        );
        let mut messages = Vec::new();
        popup.as_widget_mut().update(
            tree,
            &event,
            Layout::new(&node),
            cursor,
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(360.0, 200.0)),
        );
        messages
    }
    for replace in [false, true] {
        let mut fixture = Fixture::new(choices(3), Some(1));
        fixture.tab(false);
        fixture.tab(false);
        fixture.key(Named::Enter);
        let mut popup = fixture.retained_popup(choices(3), Some(1));
        let mut tree = Tree::new(popup.as_widget());
        if replace {
            fixture.replace(vec!["replacement".into()], Some(0));
            fixture.key(Named::Enter); // retires old source and explicitly opens current choices
        } else {
            fixture.key(Named::Escape);
            fixture.key(Named::Enter); // same choices, distinct open phase
        }
        assert!(fixture.open());
        assert_eq!(
            update(&mut popup, &mut tree, &fixture.renderer),
            [Message::SurfaceClosed]
        );
        assert!(
            update(&mut popup, &mut tree, &fixture.renderer).is_empty(),
            "old surface retires once"
        );
        assert!(
            fixture.open(),
            "retiring old popup must not close the current one"
        );
        assert_eq!(
            fixture.key(Named::Enter).0,
            [Message::Selected(if replace { 0 } else { 1 })]
        );
    }

    // The Wayland owner keeps a view provider, not just one built Element.
    // Reconstruct it only AFTER invalidation to catch borrowing a new epoch
    // while still carrying the old options and callback.
    for replace in [false, true] {
        for pointer in [false, true] {
            let mut fixture = Fixture::new(choices(3), Some(1));
            fixture.tab(false);
            fixture.tab(false);
            fixture.key(Named::Enter);
            let old_provider = fixture.retained_popup_provider(choices(3), Some(1));
            if replace {
                fixture.replace(vec!["replacement".into()], Some(0));
                fixture.key(Named::Enter);
            } else {
                fixture.key(Named::Escape);
                fixture.key(Named::Enter);
            }
            let mut rebuilt_old = old_provider();
            let mut old_tree = Tree::new(rebuilt_old.as_widget());
            let (event, cursor) = if pointer {
                (
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    mouse::Cursor::Available(cosmic::iced::Point::new(24.0, 48.0)),
                )
            } else {
                (named(Named::Enter), mouse::Cursor::Unavailable)
            };
            assert_eq!(
                update_event(
                    &mut rebuilt_old,
                    &mut old_tree,
                    &fixture.renderer,
                    event,
                    cursor
                ),
                [Message::SurfaceClosed],
                "rebuilding an obsolete native provider cannot select its old choices"
            );
            let mut rebuilt_again = old_provider();
            let mut again_tree = Tree::new(rebuilt_again.as_widget());
            assert!(
                update(&mut rebuilt_again, &mut again_tree, &fixture.renderer).is_empty(),
                "the old opening's close receipt survives native view reconstruction"
            );
            assert!(fixture.open(), "the current dropdown must remain open");
            let mut current = fixture.retained_popup(
                if replace {
                    vec!["replacement".into()]
                } else {
                    choices(3)
                },
                Some(if replace { 0 } else { 1 }),
            );
            let mut current_tree = Tree::new(current.as_widget());
            assert_eq!(
                update(&mut current, &mut current_tree, &fixture.renderer),
                [
                    Message::Selected(if replace { 0 } else { 1 }),
                    Message::SurfaceClosed
                ],
                "the current opening still selects and retires its own popup"
            );
            assert!(!fixture.open());
        }
    }
}

#[test]
fn dropdown_pointer_and_touch_select_the_current_option_after_keyboard_navigation() {
    use keyboard::key::Named;
    for touch in [false, true] {
        let mut fixture = Fixture::new(choices(3), Some(0));
        fixture.tab(false);
        fixture.tab(false);
        fixture.key(Named::Enter);
        fixture.key(Named::End);
        let (bounds, _, _) = fixture.scroll();
        let row_height =
            f32::from(cosmic::iced::widget::text::LineHeight::default().to_absolute(Pixels(14.0)))
                + 16.0;
        let point = cosmic::iced::Point::new(bounds.x + 24.0, bounds.y + 8.0 + 1.5 * row_height);
        let cursor = mouse::Cursor::Available(point);
        let event = if touch {
            Event::Touch(cosmic::iced::touch::Event::FingerPressed {
                id: cosmic::iced::touch::Finger(1),
                position: point,
            })
        } else {
            fixture.event(
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                cursor,
            );
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        };
        assert_eq!(fixture.event(event, cursor).0, [Message::Selected(1)]);
        assert!(
            !fixture.open(),
            "pointer/touch preserves exact selection and close"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn dropdown_popup_abort_and_commit_retire_only_its_own_surface_once() {
    use keyboard::key::Named;
    for (key, expected) in [
        (Named::Escape, vec![Message::SurfaceClosed]),
        (Named::Tab, vec![Message::SurfaceClosed]),
        (
            Named::Enter,
            vec![Message::Selected(1), Message::SurfaceClosed],
        ),
    ] {
        let mut fixture = Fixture::new(choices(3), Some(1));
        fixture.tab(false);
        fixture.tab(false);
        fixture.key(Named::Enter);
        let mut popup = fixture.retained_popup(choices(3), Some(1));
        let mut tree = Tree::new(popup.as_widget());
        let node = popup.as_widget_mut().layout(
            &mut tree,
            &fixture.renderer,
            &layout::Limits::new(Size::ZERO, Size::new(360.0, 200.0)),
        );
        for (event, expected) in [(named(key), expected), (named(Named::Enter), Vec::new())] {
            let mut messages = Vec::new();
            popup.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &fixture.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(360.0, 200.0)),
            );
            assert_eq!(
                messages, expected,
                "retained popup must retire exactly once"
            );
        }
    }
}

#[test]
fn dropdown_rebuild_refreshes_unselected_option_metrics_after_typography_changes() {
    use cosmic::iced::widget::text::LineHeight;
    use keyboard::key::Named;
    let options = vec![
        "x".to_owned(),
        "A much wider unselected option with iii and WWW".to_owned(),
    ];
    let build = |size, font, line_height| {
        view_with_typography(options.clone(), Some(0), size, Some(font), line_height)
    };
    let mut retained = Fixture::from_element(build(13.0, Font::DEFAULT, LineHeight::Relative(1.2)));
    retained.resize(Size::new(1600.0, 900.0));
    retained.tab(false);
    retained.tab(false);
    retained.key(Named::Enter);
    let original_width = retained.scroll().0.width;
    retained.replace_element(build(13.0, Font::DEFAULT, LineHeight::Relative(1.2)));
    assert!(
        retained.open(),
        "unchanged metrics preserve the live opening"
    );
    assert_eq!(retained.scroll().0.width, original_width);
    retained.key(Named::Escape);
    let original_id = retained.focused()[1].0.clone();
    #[cfg(target_os = "linux")]
    let mut prior_size = 13.0;

    // The labels and short selection are unchanged. Closed layout only updates
    // that selected paragraph; the open menu must refresh the long other row.
    for (size, font, line_height) in [
        (24.0, Font::DEFAULT, LineHeight::Relative(1.2)),
        (24.0, Font::MONOSPACE, LineHeight::Relative(1.2)),
        (24.0, Font::MONOSPACE, LineHeight::Absolute(Pixels(36.0))),
        (13.0, Font::DEFAULT, LineHeight::Relative(1.2)),
    ] {
        retained.key(Named::Enter);
        #[cfg(target_os = "linux")]
        let old_provider =
            retained.retained_popup_provider_with_size(options.clone(), Some(0), prior_size);
        retained.replace_element(build(size, font, line_height));
        assert_eq!(retained.focused()[1], (original_id.clone(), true));
        assert!(
            !retained.open(),
            "changed metrics retire the old opening before it can reuse captured dimensions"
        );
        retained.key(Named::Enter);
        #[cfg(target_os = "linux")]
        {
            let mut old = old_provider();
            let mut tree = Tree::new(old.as_widget());
            let viewport = Rectangle::with_size(Size::new(1600.0, 900.0));
            let node = old.as_widget_mut().layout(
                &mut tree,
                &retained.renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut messages = Vec::new();
            old.as_widget_mut().update(
                &mut tree,
                &named(Named::Enter),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &retained.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert_eq!(
                messages,
                [Message::SurfaceClosed],
                "a reconstructed old metric provider cannot activate its old choice"
            );
            assert!(
                retained.open(),
                "old retirement leaves the current opening live"
            );
        }
        let retained_bounds = retained.scroll().0;
        retained.replace_element(build(size, font, line_height));
        assert!(
            retained.open(),
            "a second unchanged reconstruction must not retire the current opening"
        );
        assert_eq!(retained.scroll().0.width, retained_bounds.width);

        let mut fresh = Fixture::from_element(build(size, font, line_height));
        fresh.resize(Size::new(1600.0, 900.0));
        fresh.tab(false);
        fresh.tab(false);
        fresh.key(Named::Enter);
        let fresh_bounds = fresh.scroll().0;
        assert_eq!(
            retained_bounds.width, fresh_bounds.width,
            "retained option width must match a newly constructed menu at current metrics"
        );
        if size == 24.0 && font == Font::DEFAULT {
            assert!(
                retained_bounds.width > original_width * 1.4,
                "the long unselected option must grow with the interface text size"
            );
        }
        assert_eq!(retained.key(Named::End).0, Vec::<Message>::new());
        assert_eq!(
            retained.key(Named::Enter).0,
            [Message::Selected(1)],
            "reflow preserves keyboard selection"
        );
        assert!(!retained.open());
        retained.key(Named::Enter);
        retained.key(Named::Escape);
        #[cfg(target_os = "linux")]
        {
            prior_size = size;
        }
    }
}
