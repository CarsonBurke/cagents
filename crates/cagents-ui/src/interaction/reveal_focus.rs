//! Scroll a scrollable so the control that takes focus inside it is in view.
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, keyboard, mouse, window};
use cosmic::{Element, Renderer, Theme};

/// `content`, a scrollable, scrolled just far enough to show a control that
/// the keyboard moves focus to inside it, by Tab, an arrow key or a shortcut
/// the app answers by focusing a control, clear of the fades at its edges.
/// A control already wholly in view stays where it is, and focus taken by
/// the pointer never scrolls: the person is looking at what they clicked,
/// and a person who scrolls the focused control away keeps the view they
/// chose.
///
/// `content` must be the scrollable itself, or wrap it without other
/// scrollables in between, as for [`super::overflow_fade_y`]. Focus inside a
/// scrollable nested in it is that scrollable's to reveal, with a
/// `reveal_focus` of its own. A control that takes focus without an id, as
/// an iced text editor made without one does, can't be followed and is left
/// where it is.
pub fn reveal_focus<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(RevealFocus {
        content: content.into(),
    })
}

struct RevealFocus<'a, Message> {
    content: Element<'a, Message>,
}

/// Where focus stood when the last key arrived.
#[derive(Default)]
struct State {
    /// The control focused before the last key was handled.
    before: Option<Id>,
    /// A key was handled since the last frame, which shows where it moved
    /// focus, if anywhere.
    pending: bool,
}

/// The outermost scrollable inside, and the focused control directly under
/// it.
#[derive(Default)]
struct FindFocus {
    /// The scrollable's viewport, its content height and its offset.
    scrollable: Option<(Rectangle, f32, f32)>,
    /// The focused control, laid out in the scrollable's content, or `None`
    /// for its bounds when it lies in a nested scrollable.
    focused: Option<(Id, Option<Rectangle>)>,
    /// How many scrollables enclose the widgets being visited.
    depth: usize,
    /// A scrollable was just visited; the next traversal enters it.
    entering: bool,
}

impl Operation for FindFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.focused.is_some() {
            return;
        }
        let entering = std::mem::take(&mut self.entering);
        self.depth += usize::from(entering);
        operate(self);
        self.depth -= usize::from(entering);
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        if self.scrollable.is_none() {
            self.scrollable = Some((bounds, content.height, translation.y));
        }
        self.entering = true;
    }

    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if let Some(id) = id.filter(|_| state.is_focused() && self.depth > 0) {
            // A nested scrollable's content is laid out unscrolled within
            // it, so these bounds say nothing about where it is drawn here.
            self.focused = Some((id.clone(), (self.depth == 1).then_some(bounds)));
        }
    }
}

/// Scrolls the outermost scrollable inside to `offset`.
struct ScrollTo(f32, bool);

impl Operation for ScrollTo {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if !self.1 {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        _content: Rectangle,
        _translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if !self.1 {
            self.1 = true;
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(self.0),
            });
        }
    }
}

impl<Message> RevealFocus<'_, Message> {
    fn find(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> FindFocus {
        let mut find = FindFocus::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut find);
        find
    }

    /// Scrolls to the focused control if focus moved to it since the last
    /// key arrived, and it isn't wholly in view.
    fn reveal(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
    ) {
        let find = self.find(tree, layout, renderer);
        let state = tree.state.downcast_mut::<State>();
        let focused = find.focused.as_ref().map(|(id, _)| id.clone());
        if state.before == focused {
            return;
        }
        state.before = focused;
        let (Some((viewport, content, current)), Some((_, Some(control)))) =
            (find.scrollable, find.focused)
        else {
            return;
        };
        let top = control.y - viewport.y;
        let height = control.height.min(viewport.height);
        if top >= current && top + height <= current + viewport.height {
            return;
        }
        let offset = super::selection_offset(top, height, viewport.height, content, current);
        if (offset - current).abs() > 0.5 {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut ScrollTo(offset, false),
            );
            shell.invalidate_layout();
            shell.request_redraw();
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for RevealFocus<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }
    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Only the keyboard's moves count: what has focus before a key is
        // the baseline, however it got there, and what has it at the next
        // frame, moved by the key itself or by the app's answer to it, is
        // the move. That frame's layout is the one the reveal measures.
        let key = matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { .. }));
        if key {
            let before = self.find(tree, layout, renderer).focused.map(|(id, _)| id);
            tree.state.downcast_mut::<State>().before = before;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if key {
            tree.state.downcast_mut::<State>().pending = true;
        } else if matches!(event, Event::Window(window::Event::RedrawRequested(_)))
            && std::mem::take(&mut tree.state.downcast_mut::<State>().pending)
        {
            self.reveal(tree, layout, renderer, shell);
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{clipboard, widget::operation::focusable};
    use cosmic::iced::{Point, time::Instant};
    use cosmic::widget;

    /// Twenty 30 px rows in a 100 px tall scrollable; the row `nested` holds
    /// a scrollable of its own, 30 px tall, around ten rows.
    struct Harness {
        element: Element<'static, ()>,
        tree: Tree,
        node: layout::Node,
        renderer: Renderer,
    }

    fn id(row: usize) -> Id {
        Id::new(format!("row-{row}"))
    }

    fn row(row: usize) -> Element<'static, ()> {
        widget::button::text(format!("Row {row}"))
            .id(id(row))
            .height(30)
            .on_press(())
            .into()
    }

    impl Harness {
        fn new(nested: Option<usize>) -> Self {
            let rows = (0..20)
                .map(|index| match nested {
                    Some(at) if at == index => widget::scrollable(widget::column::with_children(
                        (100..110).map(row).collect::<Vec<_>>(),
                    ))
                    .height(30)
                    .into(),
                    _ => row(index),
                })
                .collect::<Vec<_>>();
            let mut element =
                reveal_focus(widget::scrollable(widget::column::with_children(rows)).height(100));
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
            );
            Self {
                element,
                tree,
                node,
                renderer,
            }
        }

        fn operate(&mut self, operation: &mut dyn Operation) {
            self.element.as_widget_mut().operate(
                &mut self.tree,
                Layout::new(&self.node),
                &self.renderer,
                operation,
            );
        }

        fn event(&mut self, event: Event) {
            let mut messages = Vec::new();
            self.element.as_widget_mut().update(
                &mut self.tree,
                &event,
                Layout::new(&self.node),
                mouse::Cursor::Available(Point::ORIGIN),
                &self.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(300.0, 100.0)),
            );
        }

        fn focus(&mut self, row: usize) {
            self.operate(&mut focusable::focus::<()>(id(row)));
        }

        fn key(&mut self) {
            self.event(Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }));
        }

        fn frame(&mut self) {
            self.event(Event::Window(
                window::Event::RedrawRequested(Instant::now()),
            ));
        }

        /// The outer scrollable's offset.
        fn scrolled(&mut self) -> f32 {
            let mut find = FindFocus::default();
            self.operate(&mut find);
            find.scrollable.expect("a scrollable").2
        }
    }

    #[test]
    fn a_control_the_keyboard_focuses_below_the_fold_scrolls_into_view() {
        let mut harness = Harness::new(None);
        harness.key();
        // The app answers the key by focusing a row; the next frame shows it.
        harness.focus(10);
        harness.frame();
        let scrolled = harness.scrolled();
        assert!(
            (300.0 - 100.0 + 30.0..=300.0).contains(&scrolled),
            "row 10, at 300..330, is in the 100 px viewport at {scrolled}"
        );
        // A row already in view stays where it is.
        harness.key();
        harness.focus(9);
        harness.frame();
        assert_eq!(harness.scrolled(), scrolled);
    }

    #[test]
    fn focus_the_pointer_gives_never_scrolls() {
        let mut harness = Harness::new(None);
        // A click focuses a row partly under the fold; no key came first.
        harness.focus(3);
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
        // A later key that leaves focus where the click put it moves nothing.
        harness.key();
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
    }

    #[test]
    fn focus_inside_a_nested_scrollable_is_left_to_it() {
        let mut harness = Harness::new(Some(12));
        harness.key();
        harness.focus(105);
        harness.frame();
        assert_eq!(
            harness.scrolled(),
            0.0,
            "a nested control's unscrolled bounds don't place it in the outer view"
        );
    }
}
