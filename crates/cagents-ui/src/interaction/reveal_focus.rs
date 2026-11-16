//! Scroll a scrollable so the control that takes focus inside it is in view.
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::time::{Duration, Instant};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, keyboard, mouse, touch, window};
use cosmic::{Element, Renderer, Theme};

/// How many frames after a key the reveal waits for focus to move: the app
/// may answer the key by focusing a control a frame or two later.
const SETTLE_FRAMES: u8 = 3;

/// How long after a key the reveal waits for focus to move at most, since
/// an idle window draws no frames to count.
const SETTLE_TIME: Duration = Duration::from_millis(250);

/// `content`, a scrollable, scrolled just far enough to show a control that
/// the keyboard moves focus to inside it, by Tab, an arrow key or a shortcut
/// the app answers by focusing a control, clear of the fades at its edges.
/// Inside a [`focus_group`] the whole group shows, so a field comes with its
/// label and the line under it. Focus taken by the pointer never scrolls:
/// the person is looking at what they clicked, and a person who scrolls the
/// focused control away keeps the view they chose.
///
/// `content` must be the scrollable itself, or wrap it without other
/// scrollables in between, as for [`super::overflow_fade_y`]. Focus inside a
/// scrollable nested in it, such as a list in a dialog, brings that list
/// into view; a `reveal_focus` around the list as well then shows the
/// control within it.
pub fn reveal_focus<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Marked {
        content: content.into(),
        role: Role::Reveal,
    })
}

/// `content`, a control with the label and notes that belong to it, which a
/// [`reveal_focus`] around it shows together when the control takes focus,
/// unless the group is taller than the view.
pub fn focus_group<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Marked {
        content: content.into(),
        role: Role::Group,
    })
}

/// A wrapper that marks its content for [`FindFocus`].
struct Marked<'a, Message> {
    content: Element<'a, Message>,
    role: Role,
}

/// What a [`Marked`] wrapper says about its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// A scrollable that reveals focus within itself.
    Reveal,
    /// A control and the text about it, revealed together.
    Group,
}

/// Where focus stood when the last key arrived.
#[derive(Default)]
struct State {
    /// The control focused before the last key was handled, by its place
    /// among the controls that can take focus. Most controls are given a
    /// new id each time the view is built, and a key rebuilds it, so the
    /// place is what stays the same while focus does.
    before: Option<usize>,
    /// Frames left to wait for the last key to move focus.
    settling: u8,
    /// When the wait for the last key ends regardless.
    until: Option<Instant>,
}

impl State {
    /// Whether a key's move is still awaited at `now`.
    fn settling(&self, now: Instant) -> bool {
        self.settling > 0 && self.until.is_some_and(|until| now < until)
    }

    fn settle(&mut self) {
        self.settling = 0;
        self.until = None;
    }
}

/// A region the walk is inside.
#[derive(Clone, Copy, Debug)]
enum Frame {
    /// A scrollable's viewport.
    Scrollable(Rectangle),
    /// A [`focus_group`].
    Group(Rectangle),
}

/// The outermost scrollable inside, and what to show of the focused control.
#[derive(Default)]
struct FindFocus {
    /// The scrollable's viewport, its content height and its offset.
    scrollable: Option<(Rectangle, f32, f32)>,
    /// How many controls that can take focus the walk has passed.
    passed: usize,
    /// The focused control's place among those, and the region of the outer
    /// scrollable's content
    /// to show for it, if any.
    focused: Option<(usize, Option<Rectangle>)>,
    /// The regions enclosing the widgets being visited, outermost first.
    frames: Vec<Frame>,
    /// A region was just visited; the next traversal enters it.
    entering: Option<Frame>,
}

impl FindFocus {
    /// What to show for a control at `control`, in the outer content: the
    /// nested scrollable it is in, else its innermost group, else itself. A
    /// group taller than the view gives way to what it holds, so the control
    /// still shows; a taller nested list shows its start.
    fn target(&self, control: Rectangle) -> Option<Rectangle> {
        let mut frames = self.frames.iter();
        // The first scrollable is the outer one; regions are measured in its
        // content from there.
        let Some(Frame::Scrollable(view)) =
            frames.find(|frame| matches!(frame, Frame::Scrollable(_)))
        else {
            return None;
        };
        let fits = |region: &Rectangle| region.height <= view.height;
        let mut shown = None;
        for frame in frames {
            match *frame {
                Frame::Group(bounds) => shown = Some(bounds).filter(fits).or(shown),
                // A nested scrollable's content is laid out unscrolled
                // within it, so only its own viewport places anything here:
                // this view brings the list into view, and a `reveal_focus`
                // of the list's own shows the control within it.
                Frame::Scrollable(bounds) => {
                    return Some(
                        shown
                            .filter(|group| group.contains_rect(bounds))
                            .unwrap_or(bounds),
                    );
                }
            }
        }
        Some(shown.unwrap_or(control))
    }
}

trait ContainsRect {
    fn contains_rect(&self, other: Rectangle) -> bool;
}

impl ContainsRect for Rectangle {
    fn contains_rect(&self, other: Rectangle) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }
}

impl Operation for FindFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.focused.is_some() {
            return;
        }
        let entering = self.entering.take();
        if let Some(frame) = entering {
            self.frames.push(frame);
        }
        operate(self);
        if entering.is_some() {
            self.frames.pop();
        }
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
        self.entering = Some(Frame::Scrollable(bounds));
    }

    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(Role::Group) = state.downcast_ref::<Role>() {
            self.entering = Some(Frame::Group(bounds));
        }
    }

    fn focusable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if self.scrollable.is_none() {
            return;
        }
        if state.is_focused() {
            self.focused = Some((self.passed, self.target(bounds)));
        }
        self.passed += 1;
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

impl<Message> Marked<'_, Message> {
    fn find(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) -> FindFocus {
        let mut find = FindFocus::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut find);
        find
    }

    /// Whether focus moved since the last key arrived; if so, scrolls to
    /// what it moved to.
    fn reveal(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        let find = self.find(tree, layout, renderer);
        let state = tree.state.downcast_mut::<State>();
        let focused = find.focused.as_ref().map(|(place, _)| *place);
        if state.before == focused {
            return false;
        }
        state.before = focused;
        let (Some((viewport, content, current)), Some((_, Some(region)))) =
            (find.scrollable, find.focused)
        else {
            return true;
        };
        let top = region.y - viewport.y;
        let height = region.height.min(viewport.height);
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
        true
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Marked<'_, Message> {
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
        let content = &mut self.content;
        let child = &mut tree.children[0];
        match self.role {
            // The scrollable inside is entered by its own traversal.
            Role::Reveal => content
                .as_widget_mut()
                .operate(child, layout, renderer, operation),
            Role::Group => {
                operation.custom(None, layout.bounds(), &mut Role::Group);
                operation.traverse(&mut |operation| {
                    content
                        .as_widget_mut()
                        .operate(child, layout, renderer, operation);
                });
            }
        }
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
        if self.role == Role::Group {
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
            return;
        }
        // Only the keyboard's moves count: what has focus before a key is
        // the baseline, however it got there, and the first change in the
        // frames that follow, moved by the key itself or by the app's answer
        // to it, is the move. Those frames' layout is the one measured. The
        // pointer ends the wait: what it focuses is the person's own choice.
        let now = Instant::now();
        let key = matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { .. }));
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Touch(touch::Event::FingerPressed { .. })
        ) {
            tree.state.downcast_mut::<State>().settle();
        }
        if key && !tree.state.downcast_ref::<State>().settling(now) {
            let before = self
                .find(tree, layout, renderer)
                .focused
                .map(|(place, _)| place);
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
            let state = tree.state.downcast_mut::<State>();
            state.settling = SETTLE_FRAMES;
            state.until = Some(now + SETTLE_TIME);
        } else if let Event::Window(window::Event::RedrawRequested(at)) = event
            && tree.state.downcast_ref::<State>().settling(*at)
        {
            let moved = self.reveal(tree, layout, renderer, shell);
            let state = tree.state.downcast_mut::<State>();
            if moved {
                state.settle();
            } else {
                state.settling -= 1;
            }
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

    /// The view's height; its fades take 24 px at each edge.
    const VIEW: f32 = 150.0;

    /// What stands in a row of the page other than a 30 px button.
    #[derive(Clone, Copy)]
    enum Row {
        /// A 30 px scrollable around buttons 100 to 109.
        Nested,
        /// A button under a 20 px caption and over a 20 px note, in a
        /// [`focus_group`].
        Field,
    }

    /// Twenty rows in a [`VIEW`] tall scrollable, button rows unless `rows`
    /// says otherwise.
    struct Harness {
        rows: Vec<(usize, Row)>,
        /// Whether buttons get a new id each build, as cosmic's do unless
        /// given one.
        unique: bool,
        element: Element<'static, ()>,
        tree: Tree,
        node: layout::Node,
        renderer: Renderer,
    }

    fn id(row: usize) -> Id {
        Id::new(format!("row-{row}"))
    }

    fn limits() -> layout::Limits {
        layout::Limits::new(Size::ZERO, Size::new(300.0, VIEW))
    }

    impl Harness {
        fn new(rows: &[(usize, Row)]) -> Self {
            Self::build(rows, false)
        }

        /// Buttons without ids of their own, so each build gives them new
        /// ones.
        fn unnamed() -> Self {
            Self::build(&[], true)
        }

        fn build(rows: &[(usize, Row)], unique: bool) -> Self {
            let mut element = Self::view(rows, unique);
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits());
            Self {
                rows: rows.to_vec(),
                unique,
                element,
                tree,
                node,
                renderer,
            }
        }

        /// The view built afresh, as the app does after each message.
        fn rebuild(&mut self) {
            self.element = Self::view(&self.rows, self.unique);
            self.tree.diff(self.element.as_widget_mut());
            self.node =
                self.element
                    .as_widget_mut()
                    .layout(&mut self.tree, &self.renderer, &limits());
        }

        fn view(rows: &[(usize, Row)], unique: bool) -> Element<'static, ()> {
            let button = |row: usize| -> Element<'static, ()> {
                let button = widget::button::text(format!("Row {row}"));
                if unique { button } else { button.id(id(row)) }
                    .height(30)
                    .on_press(())
                    .into()
            };
            let rows = (0..20)
                .map(|index| {
                    match rows
                        .iter()
                        .find(|(at, _)| *at == index)
                        .map(|(_, row)| *row)
                    {
                        None => button(index),
                        Some(Row::Nested) => widget::scrollable(widget::column::with_children(
                            (100..110).map(button).collect::<Vec<_>>(),
                        ))
                        .height(30)
                        .into(),
                        Some(Row::Field) => focus_group(widget::column::with_children(vec![
                            widget::text("Caption").height(20).into(),
                            button(index),
                            widget::text("Note").height(20).into(),
                        ])),
                    }
                })
                .collect::<Vec<_>>();
            reveal_focus(widget::scrollable(widget::column::with_children(rows)).height(VIEW))
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
                &Rectangle::with_size(Size::new(300.0, VIEW)),
            );
        }

        fn focus(&mut self, row: usize) {
            self.operate(&mut focusable::focus::<()>(id(row)));
        }

        /// Focuses the `place`th control that can take focus, whatever its
        /// id.
        fn focus_place(&mut self, place: usize) {
            struct Place(usize);
            impl Operation for Place {
                fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                    operate(self);
                }
                fn focusable(
                    &mut self,
                    _id: Option<&Id>,
                    _bounds: Rectangle,
                    state: &mut dyn operation::Focusable,
                ) {
                    if self.0 == 0 {
                        state.focus();
                    } else {
                        state.unfocus();
                    }
                    self.0 = self.0.wrapping_sub(1);
                }
            }
            self.operate(&mut Place(place));
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
            self.frame_at(Instant::now());
        }

        fn frame_at(&mut self, at: Instant) {
            self.event(Event::Window(window::Event::RedrawRequested(at)));
        }

        /// A key that focuses `row`, answered in the next frame.
        fn tab_to(&mut self, row: usize) -> f32 {
            self.key();
            self.focus(row);
            self.frame();
            self.scrolled()
        }

        /// The outer scrollable's offset.
        fn scrolled(&mut self) -> f32 {
            let mut find = FindFocus::default();
            self.operate(&mut find);
            find.scrollable.expect("a scrollable").2
        }
    }

    #[test]
    fn a_control_the_keyboard_focuses_below_the_fold_scrolls_clear_of_the_fade() {
        let mut harness = Harness::new(&[]);
        // Row 10, at 300..330, ends a fade's depth above the bottom edge.
        assert_eq!(harness.tab_to(10), 330.0 + 24.0 - VIEW);
        // Rows 9 and 8, down to 240, are clear of both fades there.
        assert_eq!(harness.tab_to(9), 330.0 + 24.0 - VIEW);
        assert_eq!(harness.tab_to(8), 330.0 + 24.0 - VIEW);
        // Row 7, at 210..240, is under the top fade and comes out of it.
        assert_eq!(harness.tab_to(7), 210.0 - 24.0);
    }

    #[test]
    fn a_control_in_view_but_under_a_fade_comes_out_of_it() {
        let mut harness = Harness::new(&[]);
        // Row 4, at 120..150, is wholly inside the view, under its bottom fade.
        assert_eq!(harness.tab_to(4), 150.0 + 24.0 - VIEW);
    }

    #[test]
    fn a_field_shows_with_its_caption_and_note() {
        let mut harness = Harness::new(&[(10, Row::Field)]);
        // The group spans 300..370: caption, button, note.
        assert_eq!(harness.tab_to(10), 370.0 + 24.0 - VIEW);
        // Coming back up from row 13, the caption above the button shows
        // clear of the top fade too.
        harness.tab_to(13);
        assert_eq!(harness.tab_to(10), 300.0 - 24.0);
    }

    #[test]
    fn focus_the_pointer_gives_never_scrolls() {
        let mut harness = Harness::new(&[]);
        // A click focuses a row under the bottom fade; no key came first.
        harness.focus(4);
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
        // A later key that leaves focus where the click put it moves nothing.
        harness.key();
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
    }

    #[test]
    fn focus_inside_a_nested_list_below_the_fold_brings_the_list_into_view() {
        let mut harness = Harness::new(&[(12, Row::Nested)]);
        // The list's viewport is at 360..390; its own content, laid out
        // unscrolled, doesn't place the control in the page.
        assert_eq!(harness.tab_to(105), 390.0 + 24.0 - VIEW);
    }

    #[test]
    fn focus_the_app_moves_a_few_frames_after_the_key_is_revealed() {
        let mut harness = Harness::new(&[]);
        harness.key();
        harness.frame();
        harness.frame();
        harness.focus(10);
        harness.frame();
        assert_eq!(harness.scrolled(), 330.0 + 24.0 - VIEW);
    }

    #[test]
    fn focus_moved_long_after_the_key_is_not_the_keys() {
        let mut harness = Harness::new(&[]);
        harness.key();
        for _ in 0..SETTLE_FRAMES {
            harness.frame();
        }
        harness.focus(10);
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
    }

    #[test]
    fn a_rebuilt_view_s_new_ids_are_not_a_focus_move() {
        let mut harness = Harness::unnamed();
        // A click focuses row 4, under the bottom fade; then a key the
        // control keeps, as typing does, rebuilds the view with new ids.
        harness.focus_place(4);
        harness.frame();
        harness.key();
        harness.rebuild();
        harness.frame();
        let mut find = FindFocus::default();
        harness.operate(&mut find);
        assert_eq!(
            find.focused.map(|(place, _)| place),
            Some(4),
            "the rebuilt row keeps focus"
        );
        assert_eq!(harness.scrolled(), 0.0);
    }

    #[test]
    fn tab_through_a_rebuilt_view_still_reveals() {
        let mut harness = Harness::unnamed();
        harness.key();
        harness.rebuild();
        harness.focus_place(10);
        harness.frame();
        assert_eq!(harness.scrolled(), 330.0 + 24.0 - VIEW);
    }

    #[test]
    fn a_click_after_a_key_that_moved_nothing_doesnt_scroll() {
        let mut harness = Harness::new(&[]);
        harness.key();
        harness.frame();
        harness.event(Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        )));
        harness.focus(4);
        harness.frame();
        assert_eq!(harness.scrolled(), 0.0);
    }

    #[test]
    fn focus_moved_once_the_wait_has_timed_out_is_not_the_keys() {
        let mut harness = Harness::new(&[]);
        harness.key();
        harness.focus(10);
        harness.frame_at(Instant::now() + SETTLE_TIME);
        assert_eq!(harness.scrolled(), 0.0);
    }
}
