//! Keeping a finished exchange between frames.
//!
//! iced rebuilds, diffs and lays out the whole view after every message, so a
//! streaming answer used to pay for every exchange above it twenty times a
//! second. A retained exchange is built once for each fingerprint of what it
//! draws, its widgets are kept in its tree between frames, and its layout is
//! reused while it is given the same room.
//!
//! The same wrapper also carries an exchange that cannot be kept - one showing
//! an open step, whose read-only editor borrows from the cache - so that
//! opening or closing a step keeps the exchange's tree: the focus on the header
//! just pressed, and the scroll offsets of its code blocks, survive it.
//!
//! A widget inside that changes its own size asks for a new layout through the
//! shell, which is honored here. An overlay (a tooltip) that asks for one only
//! lays out itself, never the exchange beneath it, so it needs nothing here.
//! The application loads its fonts before any transcript is shown; a system
//! font changed while it runs reaches a kept exchange's measurements the next
//! time the exchange changes or is given different room.

use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget,
    clipboard::DndDestinationRectangles,
    layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use iced::{Event, Rectangle, Size, Vector};
use std::cell::RefCell;

type Dyn<'a> = dyn Widget<Message, cosmic::Theme, cosmic::Renderer> + 'a;

/// An exchange kept for as long as `fingerprint` names what it draws, built by
/// `build` only when it does not.
pub(super) fn kept<'a>(
    fingerprint: u64,
    build: impl Fn() -> Element<'static, Message> + 'a,
) -> Element<'a, Message> {
    Element::new(Retained {
        source: Source::Kept {
            fingerprint,
            build: Box::new(build),
        },
        pending: RefCell::new(None),
    })
}

/// An exchange drawn afresh this frame, in the same place a kept one would be.
pub(super) fn fresh<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Retained {
        source: Source::Fresh(content.into()),
        pending: RefCell::new(None),
    })
}

enum Source<'a> {
    Kept {
        fingerprint: u64,
        build: Box<dyn Fn() -> Element<'static, Message> + 'a>,
    },
    Fresh(Element<'a, Message>),
}

/// Bracket an exchange so an operation that changes descendant layout can
/// discard its cached node, including changes made during a layout pass.
pub(super) enum LayoutInvalidation {
    Enter,
    Exit { changed: bool },
}

struct Retained<'a> {
    source: Source<'a>,
    /// The tree of an element built by `state`, handed to `children`, which
    /// iced asks for next when it mounts the widget.
    pending: RefCell<Option<Tree>>,
}

/// What a kept exchange keeps: its element, what it was built for, and its
/// last layout with the limits it was computed for.
#[derive(Default)]
struct Memo {
    kept: Option<(u64, Element<'static, Message>)>,
    laid_out: Option<(layout::Limits, layout::Node)>,
}

impl Memo {
    fn fingerprint(&self) -> Option<u64> {
        self.kept.as_ref().map(|(fingerprint, _)| *fingerprint)
    }
}

impl<'a> Retained<'a> {
    /// The element this frame draws and the tree it draws from.
    fn parts<'t>(&'t mut self, tree: &'t mut Tree) -> (&'t mut Dyn<'t>, &'t mut Tree) {
        let Tree {
            state, children, ..
        } = tree;
        let child = &mut children[0];
        match &mut self.source {
            Source::Fresh(element) => (element.as_widget_mut(), child),
            Source::Kept { .. } => {
                let memo = state.downcast_mut::<Memo>();
                let (_, element) = memo.kept.as_mut().expect("a kept exchange is built");
                (element.as_widget_mut(), child)
            }
        }
    }

    fn parts_ref<'t>(&'t self, tree: &'t Tree) -> (&'t Dyn<'t>, &'t Tree) {
        let child = &tree.children[0];
        match &self.source {
            Source::Fresh(element) => (element.as_widget(), child),
            Source::Kept { .. } => {
                let memo = tree.state.downcast_ref::<Memo>();
                let (_, element) = memo.kept.as_ref().expect("a kept exchange is built");
                (element.as_widget(), child)
            }
        }
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Retained<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Memo>()
    }

    fn state(&self) -> tree::State {
        let mut memo = Memo::default();
        let child = match &self.source {
            Source::Fresh(element) => Tree::new(element),
            Source::Kept { fingerprint, build } => {
                let element = build();
                let child = Tree::new(&element);
                memo.kept = Some((*fingerprint, element));
                child
            }
        };
        *self.pending.borrow_mut() = Some(child);
        tree::State::new(memo)
    }

    fn children(&self) -> Vec<Tree> {
        vec![
            self.pending
                .borrow_mut()
                .take()
                .unwrap_or_else(|| match &self.source {
                    Source::Fresh(element) => Tree::new(element),
                    Source::Kept { .. } => Tree::empty(),
                }),
        ]
    }

    fn diff(&mut self, tree: &mut Tree) {
        let Tree {
            state, children, ..
        } = tree;
        let memo = state.downcast_mut::<Memo>();
        match &mut self.source {
            Source::Fresh(element) => {
                memo.kept = None;
                memo.laid_out = None;
                children[0].diff(element.as_widget_mut());
            }
            Source::Kept { fingerprint, build } => {
                if memo.fingerprint() == Some(*fingerprint) {
                    return;
                }
                let mut element = build();
                children[0].diff(element.as_widget_mut());
                memo.kept = Some((*fingerprint, element));
                memo.laid_out = None;
            }
        }
    }

    fn size(&self) -> Size<Length> {
        match &self.source {
            Source::Fresh(element) => element.as_widget().size(),
            // A transcript row fills the width and takes the height it needs,
            // whatever it holds; the column asks before the tree is at hand.
            Source::Kept { .. } => Size::new(Length::Fill, Length::Shrink),
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let kept = matches!(self.source, Source::Kept { .. });
        if kept
            && let Some((kept_limits, node)) = &tree.state.downcast_ref::<Memo>().laid_out
            && kept_limits == limits
        {
            let _stage = chat_stage::scope(Stage::RetainedClone);
            return node.clone();
        }
        let (element, child) = self.parts(tree);
        let node = element.layout(child, renderer, limits);
        if kept {
            tree.state.downcast_mut::<Memo>().laid_out = Some((*limits, node.clone()));
        }
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.custom(None, layout.bounds(), &mut LayoutInvalidation::Enter);
        let (element, child) = self.parts(tree);
        element.operate(child, layout, renderer, operation);
        let mut exit = LayoutInvalidation::Exit { changed: false };
        operation.custom(None, layout.bounds(), &mut exit);
        if matches!(exit, LayoutInvalidation::Exit { changed: true }) {
            tree.state.downcast_mut::<Memo>().laid_out = None;
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // The exchange runs against a shell of its own, so a relayout it asks
        // for is told apart from one a widget before it asked for.
        let mut messages = Vec::new();
        let mut inner = Shell::new(&mut messages);
        if shell.is_event_captured() {
            inner.capture_event();
        }
        let (element, child) = self.parts(tree);
        element.update(
            child, event, layout, cursor, renderer, clipboard, &mut inner, viewport,
        );
        if inner.is_layout_invalid() {
            tree.state.downcast_mut::<Memo>().laid_out = None;
        }
        shell.merge(inner, std::convert::identity);
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let (element, child) = self.parts_ref(tree);
        element.draw(child, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        let (element, child) = self.parts_ref(tree);
        element.mouse_interaction(child, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        let (element, child) = self.parts(tree);
        element.overlay(child, layout, renderer, viewport, translation)
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let (element, child) = self.parts_ref(tree);
        element.a11y_nodes(layout, child, cursor)
    }

    fn id(&self) -> Option<Id> {
        match &self.source {
            Source::Fresh(element) => element.as_widget().id(),
            Source::Kept { .. } => None,
        }
    }

    fn set_id(&mut self, id: Id) {
        if let Source::Fresh(element) = &mut self.source {
            element.as_widget_mut().set_id(id);
        }
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        dnd_rectangles: &mut DndDestinationRectangles,
    ) {
        let (element, child) = self.parts_ref(tree);
        element.drag_destinations(child, layout, renderer, dnd_rectangles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::clipboard;
    use iced::runtime::{UserInterface, user_interface::Cache};
    use std::cell::Cell;
    use std::rc::Rc;

    /// A fixed-size leaf that counts its layouts, and asks for a new one when
    /// it receives any event, the way a widget that changed its own size does.
    /// Its state is numbered when it is mounted, so a test can tell a kept
    /// tree from a new one.
    struct Counting(Rc<Cell<usize>>, Rc<Cell<usize>>);

    thread_local! {
        static MOUNTS: Cell<usize> = const { Cell::new(0) };
    }

    impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Counting {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<usize>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(MOUNTS.with(|mounts| {
                mounts.set(mounts.get() + 1);
                mounts.get()
            }))
        }

        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fixed(20.0))
        }

        fn layout(
            &mut self,
            tree: &mut Tree,
            _: &cosmic::Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            self.0.set(self.0.get() + 1);
            self.1.set(*tree.state.downcast_ref::<usize>());
            layout::Node::new(limits.resolve(Length::Fill, 20.0, Size::ZERO))
        }

        fn update(
            &mut self,
            _: &mut Tree,
            _: &Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &cosmic::Renderer,
            _: &mut dyn Clipboard,
            shell: &mut Shell<'_, Message>,
            _: &Rectangle,
        ) {
            shell.invalidate_layout();
        }

        fn draw(
            &self,
            _: &Tree,
            _: &mut cosmic::Renderer,
            _: &cosmic::Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
    }

    struct Frames {
        renderer: cosmic::Renderer,
        cache: Cache,
        layouts: Rc<Cell<usize>>,
        mount: Rc<Cell<usize>>,
    }

    impl Frames {
        fn new() -> Self {
            Self {
                renderer: crate::app::tests::headless_renderer(),
                cache: Cache::new(),
                layouts: Rc::default(),
                mount: Rc::default(),
            }
        }

        /// Builds one frame of a leaf, kept under `fingerprint` or drawn
        /// fresh without one, and says how many times it was laid out.
        fn frame(&mut self, fingerprint: Option<u64>, width: f32, events: &[Event]) -> usize {
            let before = self.layouts.get();
            let (layouts, mount) = (self.layouts.clone(), self.mount.clone());
            let element = match fingerprint {
                Some(fingerprint) => kept(fingerprint, move || {
                    Element::new(Counting(layouts.clone(), mount.clone()))
                }),
                None => fresh(Element::new(Counting(layouts, mount))),
            };
            let mut ui = UserInterface::build(
                element,
                Size::new(width, 100.0),
                std::mem::replace(&mut self.cache, Cache::new()),
                &mut self.renderer,
            );
            if !events.is_empty() {
                let _ = ui.update(
                    events,
                    mouse::Cursor::Unavailable,
                    &mut self.renderer,
                    &mut clipboard::Null,
                    &mut Vec::new(),
                );
            }
            self.cache = ui.into_cache();
            self.layouts.get() - before
        }
    }

    #[test]
    fn an_unchanged_exchange_is_laid_out_once() {
        let mut frames = Frames::new();
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 1);
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 0, "the same frame again");
        assert_eq!(
            frames.frame(Some(2), 400.0, &[]),
            1,
            "what it draws changed"
        );
        assert_eq!(
            frames.frame(Some(2), 300.0, &[]),
            1,
            "the room it has changed"
        );
        assert_eq!(frames.frame(Some(2), 300.0, &[]), 0);
    }

    #[test]
    fn a_widget_that_resizes_itself_is_laid_out_again() {
        let mut frames = Frames::new();
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 1);
        let event = Event::Mouse(mouse::Event::CursorLeft);
        // The update's own relayout, inside the same frame.
        assert_eq!(frames.frame(Some(1), 400.0, &[event]), 1);
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 0);
    }

    /// Opening a step turns a kept exchange into a fresh one and closing it
    /// turns it back; the widgets' state lives through both.
    #[test]
    fn a_kept_exchange_and_a_fresh_one_share_their_tree() {
        let mut frames = Frames::new();
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 1);
        let mounted = frames.mount.get();
        assert_eq!(
            frames.frame(None, 400.0, &[]),
            1,
            "a fresh exchange is laid out"
        );
        assert_eq!(frames.frame(None, 400.0, &[]), 1, "every frame");
        assert_eq!(frames.mount.get(), mounted);
        assert_eq!(
            frames.frame(Some(1), 400.0, &[]),
            1,
            "kept again, rebuilt once"
        );
        assert_eq!(frames.frame(Some(1), 400.0, &[]), 0);
        assert_eq!(frames.mount.get(), mounted);
    }
}
