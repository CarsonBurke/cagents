//! A subtree kept between frames for as long as the inputs it was built from
//! are unchanged.
//!
//! iced rebuilds, diffs and lays out the whole window after every message. A
//! retained subtree is built once per key, keeps its element, widget tree and
//! layout between frames, and is laid out again only when its key or the room
//! it is given changes, or when a widget inside asks for a new layout. An
//! unchanged subtree then costs a rebuild one key comparison.
//!
//! # The key is everything the subtree reads
//!
//! A subtree built from anything its key does not hold shows stale UI. Prefer
//! [`keyed`]: its builder is a plain function of the key, which captures
//! nothing, so whatever the subtree shows has to come from the key. Derive the
//! key from the application's state on every `view`, as owned data compared
//! with a derived `PartialEq`, and the subtree is rebuilt exactly when what it
//! would show changes. [`vouched`] takes a closure instead, for a key that
//! stands for its inputs, such as a revision the owner bumps; its caller is
//! responsible for that key's completeness.
//!
//! # Named state stays inside
//!
//! The subtree's widget tree lives in this widget's own state rather than
//! among its tree children. The runtime's per-frame sweep of named widget
//! states (`Tree::take_all_named`) walks every tree child, so it never enters
//! a retained subtree, and an unchanged one costs that sweep nothing. When the
//! subtree is rebuilt it reconciles its own named states, by name among its
//! own widgets, as the runtime does for the window. A named state therefore
//! does not move between a retained subtree and the rest of the window: a
//! widget whose `Id` appears on both sides of the boundary across a rebuild
//! starts afresh on the side it moved to. Keep a named widget on one side.
//!
//! Other walks over the window's widget tree stop at the boundary too. A
//! channel gesture's cancellation reaches the subtree through
//! [`crate::reorder::diff_retained`]; a parked workspace releases a retained
//! subtree as a whole, never a [`crate::ui_memory`] residency inside it.
//!
//! # What still reaches inside
//!
//! Every widget call is forwarded to the kept subtree, placed where the window
//! laid this widget out: events, drawing, the mouse cursor, overlays,
//! operations (focus, scrolling into view, find), accessibility and drag
//! destinations. The window's layout holds only this widget's size, so the
//! kept layout is never copied.

use cosmic::iced::advanced::widget::{Operation, Tree, tree};
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, clipboard::DndDestinationRectangles, layout, mouse, overlay,
    renderer,
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    /// How many times a retained subtree was actually laid out, rather than
    /// reused, on this thread.
    pub(crate) static LAYOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many times a retained subtree was built, on this thread.
    pub(crate) static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A subtree built by `build` from `key`, and kept while `key` is unchanged.
///
/// `size` is the size the subtree declares, which its parent asks for before
/// the subtree is at hand; debug builds check that the built subtree agrees.
pub(crate) fn keyed<M: 'static, K: PartialEq + 'static>(
    key: K,
    size: Size<Length>,
    build: fn(&K) -> Element<'static, M>,
) -> Element<'static, M> {
    Element::new(Retained {
        size,
        pending: RefCell::new(Some(Pending {
            key,
            build: Box::new(build),
        })),
    })
}

/// A subtree built by `build`, and kept while `key` is unchanged. The caller
/// vouches that `key` changes whenever anything `build` reads does; prefer
/// [`keyed`], which needs no such promise.
pub(crate) fn vouched<'a, M: 'static, K: PartialEq + 'static>(
    key: K,
    size: Size<Length>,
    build: impl FnOnce() -> Element<'static, M> + 'a,
) -> Element<'a, M> {
    Element::new(Retained {
        size,
        pending: RefCell::new(Some(Pending {
            key,
            build: Box::new(move |_: &K| build()),
        })),
    })
}

/// The subtree's own layout, which the window's layout leaves out.
#[cfg(test)]
pub(crate) fn content_layout<M: 'static, K: 'static>(tree: &Tree) -> &layout::Node {
    &tree
        .state
        .downcast_ref::<State<M, K>>()
        .layout
        .as_ref()
        .expect("the retained subtree is laid out")
        .node
}

type Build<'a, K, M> = Box<dyn FnOnce(&K) -> Element<'static, M> + 'a>;

/// This frame's key, and how to build the subtree for it if it is new.
struct Pending<'a, K, M> {
    key: K,
    build: Build<'a, K, M>,
}

impl<K, M> Pending<'_, K, M> {
    fn build(self) -> (K, Element<'static, M>) {
        #[cfg(test)]
        BUILDS.with(|builds| builds.set(builds.get() + 1));
        let element = (self.build)(&self.key);
        (self.key, element)
    }
}

struct Retained<'a, M, K> {
    size: Size<Length>,
    /// Taken by whichever of `state` and `diff` meets the key first. The
    /// runtime calls both for a new widget when the tree it replaces was of
    /// another kind; the second then finds the key it was built for.
    pending: RefCell<Option<Pending<'a, K, M>>>,
}

struct State<M, K> {
    key: K,
    element: Element<'static, M>,
    /// The element's tree, kept out of the tree children; see the module.
    tree: Tree,
    /// The subtree's layout, until it is rebuilt or given other room.
    layout: Option<Kept>,
    /// A widget inside asked for a new layout. Until the window lays out
    /// again, the subtree keeps using the kept one, as the window keeps
    /// using its own.
    stale: bool,
}

struct Kept {
    limits: layout::Limits,
    node: layout::Node,
}

impl Kept {
    /// The subtree's layout, placed where the window laid out this widget.
    fn place<'a>(&'a self, at: Layout<'_>) -> Layout<'a> {
        Layout::with_offset(at.position() - self.node.bounds().position(), &self.node)
            .with_virtual_offset(at.virtual_offset())
    }
}

impl<M, K> State<M, K> {
    /// Reconciles a rebuilt element with the kept tree, moving named states by
    /// name as the runtime does for the whole window, but only among this
    /// subtree's own widgets. States of widgets that are gone are dropped with
    /// the scope; the window's own extracted states, which the runtime is
    /// still reconciling around this widget, are put back untouched. The old
    /// layout goes before the new subtree is laid out, so the new layout can
    /// reuse its memory.
    fn rebuild(&mut self, key: K, mut element: Element<'static, M>) {
        use cosmic::iced::advanced::widget::tree::NAMED;
        let named = self.tree.take_all_named();
        let window = NAMED.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), named));
        self.tree.diff(element.as_widget_mut());
        cosmic::iced::runtime::user_interface::restore_cached_named_states(&mut self.tree);
        NAMED.with(|cell| *cell.borrow_mut() = window);
        self.element = element;
        self.key = key;
        self.layout = None;
        self.stale = false;
    }
}

/// Runs `update` and says whether it asked for a new layout, apart from any
/// request made before it, which is left as it was.
fn requests_layout<M>(shell: &mut Shell<'_, M>, update: impl FnOnce(&mut Shell<'_, M>)) -> bool {
    let mut earlier = false;
    shell.revalidate_layout(|| earlier = true);
    update(shell);
    let requested = shell.is_layout_invalid();
    if earlier {
        shell.invalidate_layout();
    }
    requested
}

impl<M: 'static, K: PartialEq + 'static> Retained<'_, M, K> {
    fn check_size(&self, element: &Element<'static, M>) {
        debug_assert_eq!(
            element.as_widget().size(),
            self.size,
            "a retained subtree declares the size it lays out at"
        );
    }
}

impl<M: 'static, K: PartialEq + 'static> Widget<M, Theme, Renderer> for Retained<'_, M, K> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<M, K>>()
    }

    fn state(&self) -> tree::State {
        let (key, element) = self
            .pending
            .borrow_mut()
            .take()
            .expect("a retained subtree is mounted once per frame")
            .build();
        self.check_size(&element);
        tree::State::new(State {
            key,
            tree: Tree::new(&element),
            element,
            layout: None,
            stale: false,
        })
    }

    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State<M, K>>();
        if let Some(pending) = self
            .pending
            .get_mut()
            .take_if(|pending| pending.key != state.key)
        {
            let (key, element) = pending.build();
            self.check_size(&element);
            state.rebuild(key, element);
        }
        crate::reorder::diff_retained(&mut state.tree);
    }

    fn size(&self) -> Size<Length> {
        self.size
    }

    fn size_hint(&self) -> Size<Length> {
        self.size
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State<M, K>>();
        let kept = match &state.layout {
            Some(kept) if !state.stale && kept.limits == *limits => kept,
            _ => {
                #[cfg(test)]
                LAYOUTS.with(|layouts| layouts.set(layouts.get() + 1));
                // Free the stale layout first, as the window frees its own
                // before laying out again, so the new one reuses it.
                state.layout = None;
                let node = state
                    .element
                    .as_widget_mut()
                    .layout(&mut state.tree, renderer, limits);
                state.stale = false;
                state.layout.insert(Kept {
                    limits: *limits,
                    node,
                })
            }
        };
        layout::Node::new(kept.node.size())
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State<M, K>>();
        let Some(kept) = &state.layout else {
            return;
        };
        state.element.as_widget_mut().operate(
            &mut state.tree,
            kept.place(layout),
            renderer,
            operation,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        let State {
            element,
            tree: child,
            layout: Some(kept),
            stale,
            ..
        } = tree.state.downcast_mut::<State<M, K>>()
        else {
            return;
        };
        // A widget inside may ask for a new layout, such as a hovered row
        // making room for its actions.
        *stale |= requests_layout(shell, |shell| {
            element.as_widget_mut().update(
                child,
                event,
                kept.place(layout),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        });
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State<M, K>>();
        let Some(kept) = &state.layout else {
            return mouse::Interaction::None;
        };
        state.element.as_widget().mouse_interaction(
            &state.tree,
            kept.place(layout),
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
        let state = tree.state.downcast_ref::<State<M, K>>();
        let Some(kept) = &state.layout else {
            return;
        };
        state.element.as_widget().draw(
            &state.tree,
            renderer,
            theme,
            style,
            kept.place(layout),
            cursor,
            viewport,
        );
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let state = tree.state.downcast_ref::<State<M, K>>();
        let Some(kept) = &state.layout else {
            return iced_accessibility::A11yTree::default();
        };
        state
            .element
            .as_widget()
            .a11y_nodes(kept.place(layout), &state.tree, cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut DndDestinationRectangles,
    ) {
        let state = tree.state.downcast_ref::<State<M, K>>();
        let Some(kept) = &state.layout else {
            return;
        };
        state.element.as_widget().drag_destinations(
            &state.tree,
            kept.place(layout),
            renderer,
            destinations,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, Renderer>> {
        let State {
            element,
            tree: child,
            layout: Some(kept),
            stale,
            ..
        } = tree.state.downcast_mut::<State<M, K>>()
        else {
            return None;
        };
        element
            .as_widget_mut()
            .overlay(child, kept.place(layout), renderer, viewport, translation)
            .map(|content| Watched::element(content, stale))
    }
}

/// An overlay from inside, such as a menu or tooltip, handles events apart
/// from the subtree's `update`. When it asks for a new layout, the subtree may
/// have changed with it, so the kept layout is stale too.
struct Watched<'a, M> {
    content: overlay::Element<'a, M, Theme, Renderer>,
    stale: &'a mut bool,
}

impl<'a, M: 'a> Watched<'a, M> {
    fn element(
        content: overlay::Element<'a, M, Theme, Renderer>,
        stale: &'a mut bool,
    ) -> overlay::Element<'a, M, Theme, Renderer> {
        overlay::Element::new(Box::new(Self { content, stale }))
    }
}

impl<M> overlay::Overlay<M, Theme, Renderer> for Watched<'_, M> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.content.as_overlay_mut().layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }

    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
    ) {
        let content = &mut self.content;
        *self.stale |= requests_layout(shell, |shell| {
            content
                .as_overlay_mut()
                .update(event, layout, cursor, renderer, clipboard, shell);
        });
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        layout: Layout<'b>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'b, M, Theme, Renderer>> {
        let stale = &mut *self.stale;
        self.content
            .as_overlay_mut()
            .overlay(layout, renderer)
            .map(|content| Watched::element(content, stale))
    }

    fn index(&self) -> f32 {
        self.content.as_overlay().index()
    }
}

#[cfg(test)]
mod tests;
