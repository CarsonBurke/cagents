//! Scroll a scrollable so the control that takes focus inside it is in view.
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, keyboard, mouse, window};
use cosmic::{Element, Renderer, Theme};

/// `content`, a scrollable, scrolled just far enough to show a control that
/// takes focus inside it, by Tab, an arrow key or the app, clear of the
/// fades at its edges. Only a change of focus scrolls it: a person who then
/// scrolls the focused control away keeps the view they chose.
///
/// `content` must be the scrollable itself, or wrap it without other
/// scrollables in between, as for [`super::overflow_fade_y`]. Like a focus
/// scope's stops, only controls with an id count, so selecting text in a
/// dialog's body doesn't move it.
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

/// The control last seen with focus inside.
#[derive(Default)]
struct Focused(Option<Id>);

/// The outermost scrollable inside, and the focused control under it.
#[derive(Default)]
struct FindFocus {
    /// The scrollable's viewport, its content height and its offset.
    scrollable: Option<(Rectangle, f32, f32)>,
    /// The focused control, laid out in the scrollable's content.
    focused: Option<(Id, Rectangle)>,
}

impl Operation for FindFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.focused.is_none() {
            operate(self);
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
    }

    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if let Some(id) = id.filter(|_| state.is_focused() && self.scrollable.is_some()) {
            self.focused = Some((id.clone(), bounds));
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

impl<Message> Widget<Message, Theme, Renderer> for RevealFocus<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Focused>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Focused::default())
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
        // Focus moves on a key, or between events when the app focuses a
        // control, which the next frame then shows. Pointer motion never
        // moves it, so it doesn't walk the content.
        if !matches!(
            event,
            Event::Keyboard(
                keyboard::Event::KeyPressed { .. } | keyboard::Event::KeyReleased { .. }
            ) | Event::Window(window::Event::RedrawRequested(_))
        ) {
            return;
        }
        let mut find = FindFocus::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut find);
        let focused = find.focused.as_ref().map(|(id, _)| id.clone());
        let seen = tree.state.downcast_mut::<Focused>();
        if seen.0 == focused {
            return;
        }
        seen.0 = focused;
        let (Some((viewport, content, current)), Some((_, control))) =
            (find.scrollable, find.focused)
        else {
            return;
        };
        let offset = super::selection_offset(
            control.y - viewport.y,
            control.height.min(viewport.height),
            viewport.height,
            content,
            current,
        );
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
