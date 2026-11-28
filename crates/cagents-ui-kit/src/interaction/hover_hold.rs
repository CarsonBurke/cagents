//! Holding a row or list still while the pointer rests on it.
//!
//! A strip of tabs or a list of rows that rearranges itself as what it shows
//! changes moves its items under a resting pointer: a tab closes and the
//! others widen, an agent finishes and its row leaves, a count grows and the
//! tabs after it slide along. The next click then lands on something the
//! person did not aim at. Browsers answer this for their tab strips by
//! keeping the tabs' width after a close until the pointer leaves the strip;
//! this module gives every strip and list the same convention.
//!
//! - [`hover_region`] tells the application when the pointer comes over a
//!   region and when it leaves, so it can take a [`Hold`] on what the region
//!   showed and release it afterwards.
//! - [`Held`] keeps the keys a list showed in their places, with newcomers
//!   after them; [`HeldWidth`] keeps the width a row of items shared.
//! - [`held_row`] and [`hold_extent`] hold a row's own geometry, with no
//!   application state, for rows whose items size themselves.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// What a region held when the pointer came over it, until the pointer
/// leaves or the region is no longer drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold<T>(Option<T>);

impl<T> Default for Hold<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> Hold<T> {
    /// Answers a [`hover_region`]'s report. The pointer coming over the
    /// region takes `now()` as the hold unless one is already taken; it
    /// leaving releases the hold.
    pub fn hover(&mut self, over: bool, now: impl FnOnce() -> T) {
        if !over {
            self.0 = None;
        } else if self.0.is_none() {
            self.0 = Some(now());
        }
    }

    /// Takes `value` as the hold, replacing one already taken.
    pub fn take(&mut self, value: T) {
        self.0 = Some(value);
    }

    /// Ends the hold. A region that is hidden or rebuilt elsewhere never
    /// hears the pointer leave it, so its owner releases the hold then.
    pub fn release(&mut self) {
        self.0 = None;
    }

    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.0.as_mut()
    }

    pub fn is_held(&self) -> bool {
        self.0.is_some()
    }
}

/// The keys a list showed, in order, when the pointer came over it.
///
/// [`Held::arrange`] keeps those keys in their places, whatever has become of
/// them since, and puts what arrived meanwhile after them, so nothing the
/// pointer could be resting on moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held<K>(Vec<K>);

impl<K> Default for Held<K> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: PartialEq> Held<K> {
    pub fn new(keys: impl IntoIterator<Item = K>) -> Self {
        Self(keys.into_iter().collect())
    }

    pub fn contains(&self, key: &K) -> bool {
        self.0.contains(key)
    }

    pub fn keys(&self) -> &[K] {
        &self.0
    }

    /// Holds the `keys` not yet held, after those that are, so what arrived
    /// while held keeps the place it arrived at.
    pub fn extend(&mut self, keys: impl IntoIterator<Item = K>) {
        for key in keys {
            if !self.contains(&key) {
                self.0.push(key);
            }
        }
    }

    /// The items to show while held: each held key that `find` still
    /// resolves, in its held place, then the `live` items whose keys are not
    /// held, in their live order. A held key `find` no longer resolves, such
    /// as a row that was removed, gives up its place.
    pub fn arrange<T>(
        &self,
        mut find: impl FnMut(&K) -> Option<T>,
        live: impl IntoIterator<Item = T>,
        key: impl Fn(&T) -> &K,
    ) -> Vec<T> {
        let mut items: Vec<T> = self.0.iter().filter_map(&mut find).collect();
        items.extend(live.into_iter().filter(|item| !self.contains(key(item))));
        items
    }
}

/// The width a row of items shared, such as a window's tabs, when the pointer
/// came over it, the room the row had, and which items shared the width.
///
/// While held, closing an item or retitling one keeps the others' width, so
/// the next item's close control comes to rest under the pointer, as in a
/// browser. A row showing an item it did not hold, as after a tab was added
/// or the row switched to another set of tabs, is not held: it has to make
/// room, and its owner takes a new hold at the new width (see
/// [`Self::holds`]). Nor is a row that has less room than it had, which
/// would scroll, or squeeze, at the held width.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldWidth<K> {
    width: f32,
    room: f32,
    keys: Held<K>,
}

impl<K: PartialEq> HeldWidth<K> {
    /// A hold on `width`, in a row `room` wide, shared by the items `keys`.
    pub fn new(width: f32, room: f32, keys: impl IntoIterator<Item = K>) -> Self {
        Self {
            width,
            room,
            keys: Held::new(keys),
        }
    }

    /// Whether this hold still answers for a row `room` wide showing
    /// `shown`: every item it shows was in the row when the hold was taken,
    /// and the row has no less room than it had.
    pub fn holds<'k>(&self, room: f32, shown: impl IntoIterator<Item = &'k K>) -> bool
    where
        K: 'k,
    {
        room >= self.room - 0.5 && shown.into_iter().all(|key| self.keys.contains(key))
    }

    /// The width to draw the row at: the held width while it
    /// [holds](Self::holds), else `live`, what the items would take now.
    pub fn width<'k>(&self, live: f32, room: f32, shown: impl IntoIterator<Item = &'k K>) -> f32
    where
        K: 'k,
    {
        if self.holds(room, shown) {
            self.width
        } else {
            live
        }
    }
}

/// `content`, reporting `on_hover(true)` when the pointer is over it and
/// `held` is false, and `on_hover(false)` when the pointer is off it, or
/// leaves the window, and `held` is true.
///
/// It compares the pointer with what its owner holds on every pointer event,
/// rather than remembering whether the pointer was over it. A region drawn
/// afresh, as after a page switch or a change of layout, so still releases a
/// hold taken by the one it replaced, and takes one if the pointer is
/// already over it.
pub fn hover_region<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    held: bool,
    on_hover: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Region {
        content: content.into(),
        held,
        on_hover: Box::new(on_hover),
    })
}

struct Region<'a, Message> {
    content: Element<'a, Message>,
    held: bool,
    on_hover: Box<dyn Fn(bool) -> Message + 'a>,
}

/// What a [`Region`] last reported, until its owner answers it.
///
/// Several pointer events can arrive before the owner hears the first
/// report and the region is drawn afresh with its answer. Comparing each
/// with the report already sent, rather than with the stale `held`, sends
/// each change once, and a leave right after an enter is not lost.
#[derive(Debug, Default)]
struct Reported(Option<bool>);

/// Whether `event` moves the pointer, or takes it out of the window, so a
/// region can tell whether the pointer is over it.
fn moves_pointer(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(
            mouse::Event::CursorMoved { .. }
                | mouse::Event::CursorEntered
                | mouse::Event::CursorLeft
        )
    )
}

impl<Message> Widget<Message, Theme, Renderer> for Region<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Reported>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Reported::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        // Drawn afresh, the region carries its owner's answer in `held`.
        if tree.tag == tree::Tag::of::<Reported>() {
            tree.state.downcast_mut::<Reported>().0 = None;
        }
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
        if moves_pointer(event) {
            // A menu or tooltip the region opened, under the pointer, does
            // not take the pointer off the region.
            let over = !matches!(event, Event::Mouse(mouse::Event::CursorLeft))
                && cursor.land().is_over(layout.bounds());
            let reported = &mut tree.state.downcast_mut::<Reported>().0;
            if over != reported.unwrap_or(self.held) {
                *reported = Some(over);
                shell.publish((self.on_hover)(over));
            }
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

/// What a [`held_row`] or [`hold_extent`] holds of its own geometry.
#[derive(Debug, Default)]
struct Geometry {
    /// Whether the pointer was over the row at its last pointer event.
    over: bool,
    /// The children's widths, or the content's, when the pointer came over.
    widths: Option<Vec<f32>>,
    /// Which children are drawn narrower than they would like while held,
    /// and so are clipped to their held width.
    clipped: Vec<bool>,
}

impl Geometry {
    /// Notes whether the pointer is over `bounds` after `event`. Its leaving
    /// releases the held geometry and lays the row out afresh.
    fn track<Message>(
        &mut self,
        event: &Event,
        cursor: mouse::Cursor,
        bounds: Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        if !moves_pointer(event) {
            return;
        }
        let over =
            !matches!(event, Event::Mouse(mouse::Event::CursorLeft)) && cursor.is_over(bounds);
        if self.over && !over {
            self.widths = None;
            shell.invalidate_layout();
        }
        self.over = over;
    }
}

/// A row of `children`, `spacing` apart and centred across the row, that
/// keeps each child's width while the pointer is over it.
///
/// For a row of tabs whose labels carry counts, such as a pane's view tabs:
/// a count that appears, grows or goes no longer moves the tabs after it
/// under the pointer. A child that would grow is drawn at its held width,
/// clipped, until the pointer leaves and the row settles. A row that gains
/// or loses a child is laid out afresh, as that is its owner's own change.
pub fn held_row<'a, Message: 'a>(
    children: Vec<Element<'a, Message>>,
    spacing: f32,
) -> Element<'a, Message> {
    Element::new(HeldRow { children, spacing })
}

struct HeldRow<'a, Message> {
    children: Vec<Element<'a, Message>>,
    spacing: f32,
}

impl<Message> Widget<Message, Theme, Renderer> for HeldRow<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Geometry>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Geometry::default())
    }
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let max = limits.max();
        let geometry = tree.state.downcast_mut::<Geometry>();
        let held = geometry
            .widths
            .take()
            .filter(|widths| geometry.over && widths.len() == self.children.len());
        let mut x = 0.0;
        let mut height: f32 = 0.0;
        let mut widths = Vec::with_capacity(self.children.len());
        let mut clipped = Vec::with_capacity(self.children.len());
        let mut nodes = Vec::with_capacity(self.children.len());
        for (index, (child, child_tree)) in
            self.children.iter_mut().zip(&mut tree.children).enumerate()
        {
            let room = Size::new((max.width - x).max(0.0), max.height);
            let natural = child.as_widget_mut().layout(
                child_tree,
                renderer,
                &layout::Limits::new(Size::ZERO, room),
            );
            let node = match held.as_ref().map(|widths| widths[index]) {
                Some(width) if (width - natural.size().width).abs() > 0.5 => {
                    let fixed = Size::new(width, room.height);
                    let node = child.as_widget_mut().layout(
                        child_tree,
                        renderer,
                        &layout::Limits::new(Size::new(width, 0.0), fixed),
                    );
                    clipped.push(natural.size().width > width);
                    node
                }
                _ => {
                    clipped.push(false);
                    natural
                }
            };
            widths.push(node.size().width);
            height = height.max(node.size().height);
            nodes.push(node.move_to(Point::new(x, 0.0)));
            x += widths[index] + self.spacing;
        }
        for node in &mut nodes {
            let y = ((height - node.size().height) / 2.0).max(0.0);
            let at = node.bounds().position();
            node.move_to_mut(Point::new(at.x, y));
        }
        let width = (x - self.spacing).max(0.0);
        let geometry = tree.state.downcast_mut::<Geometry>();
        // Taken as the pointer arrives; kept as it stays.
        geometry.widths = geometry.over.then_some(held.unwrap_or(widths));
        geometry.clipped = clipped;
        layout::Node::with_children(
            limits.resolve(Length::Shrink, Length::Shrink, Size::new(width, height)),
            nodes,
        )
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, tree), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    tree,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
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
        let geometry = tree.state.downcast_mut::<Geometry>();
        let was_over = geometry.over;
        geometry.track(event, cursor, layout.bounds(), shell);
        if !was_over && geometry.over {
            // The widths drawn now are the ones to hold.
            geometry.widths = Some(
                layout
                    .children()
                    .map(|child| child.bounds().width)
                    .collect(),
            );
        }
        for ((child, tree), child_layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                tree,
                event,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
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
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, tree), child_layout)| {
                child.as_widget().mouse_interaction(
                    tree,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
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
        use cosmic::iced::advanced::Renderer as _;
        let clipped = &tree.state.downcast_ref::<Geometry>().clipped;
        for (index, ((child, tree), child_layout)) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .enumerate()
        {
            let child_layout = child_layout.with_virtual_offset(layout.virtual_offset());
            if clipped.get(index).copied().unwrap_or(false) {
                let Some(clip) = child_layout.bounds().intersection(viewport) else {
                    continue;
                };
                renderer.with_layer(clip, |renderer| {
                    child.as_widget().draw(
                        tree,
                        renderer,
                        theme,
                        style,
                        child_layout,
                        cursor,
                        &clip,
                    );
                });
            } else {
                child.as_widget().draw(
                    tree,
                    renderer,
                    theme,
                    style,
                    child_layout,
                    cursor,
                    viewport,
                );
            }
        }
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
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
        iced_accessibility::A11yTree::join(
            self.children
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
                .map(|((child, tree), child_layout)| {
                    child.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        tree,
                        cursor,
                    )
                }),
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().drag_destinations(
                tree,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

/// `content`, kept at least as wide as it has been since the pointer came
/// over it, until the pointer leaves.
///
/// For the content of a horizontally scrolling strip: a tab closed near the
/// end of a scrolled strip would otherwise shorten the content, the strip
/// would clamp its offset, and every tab would slide under the pointer. The
/// strip keeps the room instead, and settles once the pointer leaves.
pub fn hold_extent<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Extent {
        content: content.into(),
    })
}

struct Extent<'a, Message> {
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, Renderer> for Extent<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Geometry>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Geometry::default())
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
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let geometry = tree.state.downcast_mut::<Geometry>();
        let size = child.size();
        let width = match geometry.widths.as_deref() {
            Some([held]) if geometry.over => size.width.max(*held),
            _ => size.width,
        };
        geometry.widths = geometry.over.then(|| vec![width]);
        layout::Node::with_children(Size::new(width, size.height), vec![child])
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let child = first_child(layout);
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], child, renderer, operation);
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
        let geometry = tree.state.downcast_mut::<Geometry>();
        let was_over = geometry.over;
        geometry.track(event, cursor, layout.bounds(), shell);
        if !was_over && geometry.over {
            geometry.widths = Some(vec![layout.bounds().width]);
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            first_child(layout),
            cursor,
            renderer,
            clipboard,
            shell,
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            first_child(layout),
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
            first_child(layout),
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
            first_child(layout),
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
            .a11y_nodes(first_child(layout), &tree.children[0], cursor)
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
            first_child(layout),
            renderer,
            destinations,
        );
    }
}

/// The one child of a wrapper's layout, at the wrapper's virtual offset.
fn first_child(layout: Layout<'_>) -> Layout<'_> {
    layout
        .children()
        .next()
        .expect("a wrapper lays out its content")
        .with_virtual_offset(layout.virtual_offset())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use cosmic::iced::advanced::clipboard;
    use cosmic::widget;

    #[test]
    fn a_hold_is_taken_once_and_released_when_the_pointer_leaves() {
        let mut hold = Hold::default();
        hold.hover(true, || 1);
        // A second report while held keeps what the pointer arrived to.
        hold.hover(true, || 2);
        assert_eq!(hold.get(), Some(&1));
        hold.hover(false, || 3);
        assert!(!hold.is_held());
        hold.take(4);
        hold.release();
        assert_eq!(hold.get(), None);
    }

    #[test]
    fn held_keys_keep_their_places_and_newcomers_follow_them() {
        let held = Held::new(["b", "a", "gone"]);
        // Live order changed, "c" arrived, "gone" was removed, and "a" is no
        // longer live but still exists.
        let all = ["a", "b", "c", "d"];
        let arranged = held.arrange(
            |key| all.iter().find(|item| *item == key).copied(),
            ["c", "b", "d"],
            |item| item,
        );
        assert_eq!(arranged, ["b", "a", "c", "d"]);
    }

    #[test]
    fn a_held_width_survives_closes_but_not_newcomers_or_lost_room() {
        let held = HeldWidth::new(120.0, 600.0, [1, 2, 3, 4]);
        // A close or a retitle: the live width changed, the held one stays.
        assert_eq!(held.width(200.0, 600.0, &[1, 2, 4]), 120.0);
        assert_eq!(held.width(90.0, 600.0, &[1, 2, 3, 4]), 120.0);
        // A wider window keeps the hold; the tabs still fit.
        assert_eq!(held.width(200.0, 800.0, &[1, 2, 4]), 120.0);
        // An addition, or another set of tabs, is the owner's own change:
        // the row makes room.
        assert!(!held.holds(600.0, &[1, 2, 3, 4, 5]));
        assert_eq!(held.width(100.0, 600.0, &[1, 2, 3, 4, 5]), 100.0);
        assert_eq!(held.width(100.0, 600.0, &[7, 8]), 100.0);
        // A row that lost room gives way, and stays given way however
        // many tabs close after.
        assert_eq!(held.width(100.0, 500.0, &[1, 2, 3]), 100.0);
        assert_eq!(held.width(150.0, 500.0, &[1]), 150.0);
    }

    /// Runs `events` through `element`, laid out in a 200x100 window, with
    /// the pointer at `pointer`, and returns what it published.
    fn run<M: Clone>(
        element: &mut Element<'_, M>,
        tree: &mut Tree,
        events: &[(Event, mouse::Cursor)],
    ) -> Vec<M> {
        let renderer = headless_renderer();
        let limits = layout::Limits::new(Size::ZERO, Size::new(200.0, 100.0));
        let node = element.as_widget_mut().layout(tree, &renderer, &limits);
        let mut messages = Vec::new();
        for (event, cursor) in events {
            element.as_widget_mut().update(
                tree,
                event,
                Layout::new(&node),
                *cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(200.0, 100.0)),
            );
        }
        messages
    }

    fn moved(x: f32, y: f32) -> (Event, mouse::Cursor) {
        let position = Point::new(x, y);
        (
            Event::Mouse(mouse::Event::CursorMoved { position }),
            mouse::Cursor::Available(position),
        )
    }

    #[test]
    fn a_region_reports_against_its_owners_hold_even_when_drawn_afresh() {
        let region = |held| -> Element<'static, bool> {
            hover_region(
                widget::Space::new().width(100).height(40),
                held,
                std::convert::identity,
            )
        };
        let mut element = region(false);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(
            run(
                &mut element,
                &mut tree,
                &[moved(150.0, 20.0), moved(50.0, 20.0)]
            ),
            [true],
            "the pointer arriving takes a hold"
        );
        // Drawn afresh, as after a remount, while the owner still holds:
        // the first pointer event off the region releases it, though this
        // region never saw the pointer arrive.
        let mut element = region(true);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(run(&mut element, &mut tree, &[moved(150.0, 20.0)]), [false]);
        let mut element = region(true);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(
            run(
                &mut element,
                &mut tree,
                &[
                    moved(50.0, 20.0),
                    (
                        Event::Mouse(mouse::Event::CursorLeft),
                        mouse::Cursor::Unavailable
                    )
                ]
            ),
            [false],
            "leaving the window releases the hold"
        );
        // Events that arrive before the owner answers report each change
        // once, and a leave right after an enter is not lost.
        let mut element = region(false);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(
            run(
                &mut element,
                &mut tree,
                &[
                    moved(50.0, 20.0),
                    moved(60.0, 20.0),
                    moved(150.0, 20.0),
                    moved(160.0, 20.0)
                ]
            ),
            [true, false]
        );
    }

    fn widths(element: &mut Element<'_, ()>, tree: &mut Tree) -> Vec<f32> {
        let renderer = headless_renderer();
        let node = element.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 100.0)),
        );
        Layout::new(&node)
            .children()
            .map(|child| child.bounds().width)
            .collect()
    }

    #[test]
    fn a_held_row_keeps_its_childrens_widths_until_the_pointer_leaves() {
        let row = |first: f32| -> Element<'static, ()> {
            held_row(
                vec![
                    widget::Space::new().width(first).height(20).into(),
                    widget::Space::new().width(40).height(20).into(),
                ],
                4.0,
            )
        };
        let mut element = row(30.0);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(widths(&mut element, &mut tree), [30.0, 40.0]);
        run(&mut element, &mut tree, &[moved(10.0, 10.0)]);
        // A count appears in the first tab and one goes from it again.
        for first in [50.0, 20.0] {
            let mut element = row(first);
            tree.diff(element.as_widget_mut());
            assert_eq!(widths(&mut element, &mut tree), [30.0, 40.0]);
        }
        let mut element = row(50.0);
        tree.diff(element.as_widget_mut());
        widths(&mut element, &mut tree);
        run(&mut element, &mut tree, &[moved(10.0, 90.0)]);
        assert_eq!(widths(&mut element, &mut tree), [50.0, 40.0]);
    }

    #[test]
    fn a_held_extent_keeps_the_room_a_strip_had() {
        let content = |width: f32| -> Element<'static, ()> {
            hold_extent(widget::Space::new().width(width).height(20))
        };
        let size = |element: &mut Element<'_, ()>, tree: &mut Tree| {
            let renderer = headless_renderer();
            element
                .as_widget_mut()
                .layout(
                    tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
                )
                .size()
                .width
        };
        let mut element = content(180.0);
        let mut tree = Tree::new(element.as_widget());
        assert_eq!(size(&mut element, &mut tree), 180.0);
        run(&mut element, &mut tree, &[moved(170.0, 10.0)]);
        let mut element = content(120.0);
        tree.diff(element.as_widget_mut());
        assert_eq!(
            size(&mut element, &mut tree),
            180.0,
            "a close keeps the room"
        );
        run(&mut element, &mut tree, &[moved(170.0, 60.0)]);
        assert_eq!(
            size(&mut element, &mut tree),
            120.0,
            "and settles once left"
        );
    }
}
