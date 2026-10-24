//! Explicit residency boundaries for disposable presentation widget state.
//!
//! A parked workspace tree preserves inputs and other interactive state. A
//! gated subtree instead rebuilds from its model when mounted again; this
//! releases retained elements, shaping buffers and layout caches while hidden.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget,
    clipboard::DndDestinationRectangles,
    layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// Mark presentation state that can be discarded while its workspace card is
/// parked. Logical reader state belongs in the model outside this boundary.
pub fn gate<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Gate(content.into()))
}

struct Gate<'a, Message>(Element<'a, Message>);
struct Residency;

/// Called after reclaiming named state, before parking a tree. Traversal stops
/// at each boundary: dropping its children releases the whole presentation.
pub(crate) fn release_parked(tree: &mut Tree) {
    trace_allocator("before_workspace_park");
    if release_parked_children(tree) {
        trace_global_font_sources();
    }
}

fn release_parked_children(tree: &mut Tree) -> bool {
    if tree.tag == tree::Tag::of::<Residency>() {
        let released = !tree.children.is_empty();
        tree.children = Vec::new();
        released
    } else {
        let mut released = false;
        for child in &mut tree.children {
            released |= release_parked_children(child);
        }
        released
    }
}

/// Observe the global DB's existing shared mappings, without requesting a font.
/// The native route has already laid out text before parking. The public iced
/// accessor is lazy, so a synthetic TRACE-enabled caller before any text layout
/// can initialize that DB; this is not a general non-initialization guarantee.
fn trace_global_font_sources() {
    if !tracing::enabled!(target: "cagents::memory", tracing::Level::TRACE) {
        return;
    }
    use cosmic::iced::advanced::graphics::text::{self, cosmic_text::fontdb};
    use std::sync::Arc;

    // Copy only primitive observations. In particular, do not clone a font or
    // its shared-data Arc, call get_font, or keep the font lock while emitting.
    let snapshot = text::font_system().try_read().ok().map(|fonts| {
        fonts
            .db()
            .faces()
            .filter_map(|face| {
                let fontdb::Source::SharedFile(_, data) = &face.source else {
                    return None;
                };
                let bytes: &[u8] = data.as_ref().as_ref();
                Some((
                    face.id,
                    bytes.as_ptr() as usize,
                    bytes.len(),
                    Arc::strong_count(data),
                ))
            })
            .collect::<Vec<_>>()
    });
    let Some(sources) = snapshot else {
        tracing::trace!(
            target: "cagents::memory",
            process_id = std::process::id(),
            owner = "global_font_sources",
            status = "unknown_lock_unavailable",
        );
        return;
    };
    tracing::trace!(
        target: "cagents::memory",
        process_id = std::process::id(),
        owner = "global_font_sources",
        status = "observed",
        shared_face_references = sources.len(),
    );
    for (face_id, source_base, mapped_bytes, shared_references) in sources {
        // Several faces may share one source. Neither mapped length nor Arc
        // count is resident/reclaimable bytes; raw smaps provides that join.
        tracing::trace!(
            target: "cagents::memory",
            process_id = std::process::id(),
            owner = "global_font_source",
            ?face_id,
            source_base,
            mapped_bytes,
            shared_references,
        );
    }
}

/// Read allocator accounting only at existing presentation retirement boundaries.
/// These are reserved/used allocation bytes, not resident or reclaimable bytes;
/// native smaps observations supply that separate measurement.
pub(crate) fn trace_allocator(boundary: &'static str) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        if !tracing::enabled!(target: "cagents::memory", tracing::Level::TRACE) {
            return;
        }
        type Mallinfo2 = unsafe extern "C" fn() -> libc::mallinfo2;
        static OBSERVE: std::sync::OnceLock<Option<Mallinfo2>> = std::sync::OnceLock::new();
        let Some(observe) = OBSERVE.get_or_init(|| {
            // Resolve optionally: a diagnostic must not require GLIBC_2.33 on
            // systems which supported this executable before it was added.
            let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallinfo2".as_ptr()) };
            if symbol.is_null() {
                None
            } else {
                // SAFETY: the GNU libc symbol has this exact C signature; its
                // library remains loaded for the process lifetime.
                Some(unsafe { std::mem::transmute::<*mut libc::c_void, Mallinfo2>(symbol) })
            }
        }) else {
            tracing::trace!(
                target: "cagents::memory",
                owner = "glibc_allocator",
                boundary,
                status = "unavailable",
            );
            return;
        };
        // SAFETY: the resolved function has no pointer arguments and returns a
        // value snapshot. It neither releases pages nor changes allocator policy.
        let info = unsafe { observe() };
        tracing::trace!(
            target: "cagents::memory",
            process_id = std::process::id(),
            owner = "glibc_allocator",
            boundary,
            status = "observed",
            arena_bytes = info.arena,
            used_arena_bytes = info.uordblks,
            free_arena_bytes = info.fordblks,
            mmap_bytes = info.hblkhd,
        );
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = boundary;
}

impl<Message> Widget<Message, Theme, Renderer> for Gate<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Residency>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Residency)
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.0)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.0));
    }

    fn size(&self) -> Size<Length> {
        self.0.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.0.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.0
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
        self.0
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
        self.0.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
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
        self.0.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.0
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.0.as_widget_mut().overlay(
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
        self.0
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        rectangles: &mut DndDestinationRectangles,
    ) {
        self.0
            .as_widget()
            .drag_destinations(&tree.children[0], layout, renderer, rectangles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Weak};

    struct Probe(Rc<RefCell<Weak<()>>>);

    impl Widget<(), Theme, Renderer> for Probe {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<Arc<()>>()
        }

        fn state(&self) -> tree::State {
            let owner = Arc::new(());
            *self.0.borrow_mut() = Arc::downgrade(&owner);
            tree::State::new(owner)
        }

        fn size(&self) -> Size<Length> {
            Size::new(Length::Shrink, Length::Shrink)
        }

        fn layout(&mut self, _: &mut Tree, _: &Renderer, _: &layout::Limits) -> layout::Node {
            layout::Node::new(Size::ZERO)
        }

        fn draw(
            &self,
            _: &Tree,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
    }

    #[test]
    fn parking_drops_only_disposable_state_and_remount_rebuilds_it() {
        let presentation = Rc::new(RefCell::new(Weak::new()));
        let composer = Rc::new(RefCell::new(Weak::new()));
        let mut gated = gate(Element::new(Probe(presentation.clone())));
        let input = Element::new(Probe(composer.clone()));
        let mut root = Tree::empty();
        root.children = vec![Tree::new(&gated), Tree::new(&input)];
        let old_presentation = presentation.borrow().clone();
        let old_composer = composer.borrow().clone();
        assert!(old_presentation.upgrade().is_some());
        assert!(
            root.children[0].id.is_none(),
            "the gate never aliases its child's widget ID"
        );
        release_parked(&mut root);
        assert!(old_presentation.upgrade().is_none());
        assert!(old_composer.upgrade().is_some());
        // Repeated parking is harmless; remount runs normal child diffing.
        release_parked(&mut root);
        root.children[0].diff(&mut gated);
        assert!(presentation.borrow().upgrade().is_some());
        assert!(old_presentation.upgrade().is_none());
        assert!(old_composer.upgrade().is_some());
    }
}
