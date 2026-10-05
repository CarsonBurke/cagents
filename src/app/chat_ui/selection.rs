//! One selection across the transcript's independently shaped prose widgets.
//! Operations discover the current widget tree, including retained exchanges;
//! endpoints use widget identities so reflow never changes what was selected.
use super::selectable_prose::State as Prose;
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};
use std::any::Any;
use std::ops::Range;
use std::sync::atomic::AtomicU64;

static NEXT: AtomicU64 = AtomicU64::new(1);
static ACTIVE: AtomicU64 = AtomicU64::new(0);

pub(super) fn transcript<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
) -> Element<'a, Message> {
    Element::new(Selection {
        content: content.into(),
        context: Some((target, subagent)),
    })
}

struct Selection<'a> {
    content: Element<'a, Message>,
    context: Option<(Target, Option<&'a str>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Endpoint {
    identity: u64,
    byte: usize,
}

struct State {
    identity: u64,
    context: Option<(Target, Option<String>)>,
    reset: bool,
    anchor: Option<Endpoint>,
    end: Option<Endpoint>,
    dragging: bool,
    focused: bool,
    link: Option<(Target, String)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            identity: NEXT.fetch_add(1, Ordering::Relaxed),
            context: None,
            reset: false,
            anchor: None,
            end: None,
            dragging: false,
            focused: false,
            link: None,
        }
    }
}

impl operation::Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
        ACTIVE.store(self.identity, Ordering::Relaxed);
    }
    fn unfocus(&mut self) {
        self.focused = false;
        self.dragging = false;
        self.link = None;
    }
}

struct Block {
    identity: u64,
    plain: Arc<str>,
    bounds: Rectangle,
    visible: Option<Rectangle>,
    hit: usize,
    link: Option<(Target, String)>,
}

struct Read {
    blocks: Vec<Block>,
    pointer: Option<Point>,
    translation: Vector,
    pending: Option<(Vector, Rectangle)>,
    clip: Option<Rectangle>,
}

impl Read {
    fn new(pointer: Option<Point>) -> Self {
        Self {
            blocks: Vec::new(),
            pointer,
            translation: Vector::ZERO,
            pending: None,
            clip: None,
        }
    }

    fn endpoint(&self, inside: bool) -> Option<&Block> {
        let point = self.pointer?;
        let distance = |bounds: Rectangle| {
            let dx = (bounds.x - point.x)
                .max(0.0)
                .max(point.x - bounds.x - bounds.width);
            let dy = (bounds.y - point.y)
                .max(0.0)
                .max(point.y - bounds.y - bounds.height);
            dx * dx + dy * dy
        };
        self.blocks
            .iter()
            .filter(|block| !inside || block.visible.is_some_and(|visible| visible.contains(point)))
            .min_by(|a, b| distance(a.bounds).total_cmp(&distance(b.bounds)))
    }

    fn ranges(&self, anchor: Endpoint, end: Endpoint) -> Option<Vec<Range<usize>>> {
        let locate = |point: Endpoint| {
            self.blocks
                .iter()
                .position(|block| {
                    block.identity == point.identity && block.plain.is_char_boundary(point.byte)
                })
                .map(|index| (index, point.byte))
        };
        let anchor = locate(anchor)?;
        let end = locate(end)?;
        let (lo, hi) = if anchor <= end {
            (anchor, end)
        } else {
            (end, anchor)
        };
        Some(
            self.blocks
                .iter()
                .enumerate()
                .map(|(index, block)| {
                    if index < lo.0 || index > hi.0 {
                        0..0
                    } else {
                        let start = if index == lo.0 { lo.1 } else { 0 };
                        let end = if index == hi.0 {
                            hi.1
                        } else {
                            block.plain.len()
                        };
                        start..end
                    }
                })
                .collect(),
        )
    }

    fn copy(&self, ranges: &[Range<usize>]) -> String {
        self.blocks
            .iter()
            .zip(ranges)
            .filter(|(_, range)| !range.is_empty())
            .map(|(block, range)| &block.plain[range.clone()])
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl Operation for Read {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let previous = self.translation;
        let clip = self.clip;
        if let Some((translation, bounds)) = self.pending.take() {
            let bounds = Rectangle {
                x: bounds.x - self.translation.x,
                y: bounds.y - self.translation.y,
                ..bounds
            };
            self.clip = Some(self.clip.map_or(bounds, |clip| {
                clip.intersection(&bounds).unwrap_or_default()
            }));
            self.translation += translation;
        }
        operate(self);
        self.translation = previous;
        self.clip = clip;
    }

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.pending = Some((translation, bounds));
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        let Some(prose) = state.downcast_mut::<Prose>() else {
            return;
        };
        prose.managed = true;
        let bounds = Rectangle {
            x: bounds.x - self.translation.x,
            y: bounds.y - self.translation.y,
            ..bounds
        };
        let position = self
            .pointer
            .map(|point| point - Vector::new(bounds.x, bounds.y));
        self.blocks.push(Block {
            identity: prose.identity,
            plain: prose.plain.clone(),
            bounds,
            visible: self
                .clip
                .map_or(Some(bounds), |clip| bounds.intersection(&clip)),
            hit: position.map_or(0, |point| prose.hit(point)),
            link: position
                .filter(|_| self.pointer.is_some_and(|point| bounds.contains(point)))
                .and_then(|point| prose.target.zip(prose.link(point))),
        });
    }
}

struct Apply<'a> {
    ranges: &'a [Range<usize>],
    index: usize,
    changed: usize,
    exchanges: Vec<usize>,
}
impl Operation for Apply<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
        if let Some(invalidation) = state.downcast_mut::<retained::LayoutInvalidation>() {
            match invalidation {
                retained::LayoutInvalidation::Enter => self.exchanges.push(self.changed),
                retained::LayoutInvalidation::Exit { changed } => {
                    *changed = self
                        .exchanges
                        .pop()
                        .is_some_and(|before| before != self.changed);
                }
            }
            return;
        }
        let Some(prose) = state.downcast_mut::<Prose>() else {
            return;
        };
        let range = self.ranges.get(self.index).cloned().unwrap_or(0..0);
        self.index += 1;
        if (prose.anchor, prose.end) != (range.start, range.end) {
            prose.anchor = range.start;
            prose.end = range.end;
            // Propagated through each exchange's update shell, which also
            // invalidates its retained layout. Only changed exchanges reflow.
            prose.dirty = true;
            self.changed += 1;
        }
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Selection<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State {
            context: self
                .context
                .map(|(target, subagent)| (target, subagent.map(str::to_owned))),
            ..State::default()
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state
            .context
            .as_ref()
            .map(|(target, subagent)| (*target, subagent.as_deref()))
            != self.context
        {
            *state = State {
                context: self
                    .context
                    .map(|(target, subagent)| (target, subagent.map(str::to_owned))),
                reset: true,
                ..State::default()
            };
        }
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
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let state = tree.state.downcast_mut::<State>();
        if state.anchor.is_some() || std::mem::take(&mut state.reset) {
            let mut read = Read::new(None);
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut read,
            );
            let ranges = state
                .anchor
                .zip(state.end)
                .and_then(|(anchor, end)| read.ranges(anchor, end));
            if ranges.is_none() {
                state.anchor = None;
                state.end = None;
                state.dragging = false;
                state.link = None;
            }
            let mut apply = Apply {
                ranges: ranges.as_deref().unwrap_or(&[]),
                index: 0,
                changed: 0,
                exchanges: Vec::new(),
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut apply,
            );
            if apply.changed > 0 {
                return self.content.as_widget_mut().layout(
                    &mut tree.children[0],
                    renderer,
                    limits,
                );
            }
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
        operation.focusable(None, layout.bounds(), tree.state.downcast_mut::<State>());
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
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let press = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        );
        let motion = matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. }));
        let release = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        );
        let command = match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key),
                modifiers,
                ..
            }) if state.focused
                && ACTIVE.load(Ordering::Relaxed) == state.identity
                && modifiers.command()
                && !shell.is_event_captured()
                && (key.eq_ignore_ascii_case("c") || key.eq_ignore_ascii_case("a")) =>
            {
                Some(key.as_str())
            }
            _ => None,
        };
        let mut captured = false;
        if press || (state.dragging && (motion || release)) || command.is_some() {
            let mut read = Read::new(cursor.position());
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut read,
            );
            if press {
                state.focused = cursor.is_over(layout.bounds())
                    && cursor.is_over(*viewport)
                    && read.endpoint(true).is_some()
                    && !shell.is_event_captured();
                state.dragging = state.focused;
                state.anchor = if state.focused {
                    let block = read.endpoint(true).unwrap();
                    state.link = block.link.clone();
                    ACTIVE.store(state.identity, Ordering::Relaxed);
                    Some(Endpoint {
                        identity: block.identity,
                        byte: block.hit,
                    })
                } else {
                    state.link = None;
                    None
                };
                state.end = state.anchor;
                captured = state.focused;
            } else if state.dragging {
                if let Some(block) = read.endpoint(false) {
                    state.end = Some(Endpoint {
                        identity: block.identity,
                        byte: block.hit,
                    });
                }
                if release {
                    state.dragging = false;
                    if state.anchor == state.end
                        && let Some((target, link)) = state.link.take()
                        && read.endpoint(true).is_some_and(|block| {
                            block.link.as_ref() == Some(&(target, link.clone()))
                        })
                    {
                        shell.publish(action(Action::OpenDocument(target, link)));
                    }
                }
                captured = true;
            } else if command.is_some_and(|key| key.eq_ignore_ascii_case("a")) {
                state.anchor = read.blocks.first().map(|block| Endpoint {
                    identity: block.identity,
                    byte: 0,
                });
                state.end = read.blocks.last().map(|block| Endpoint {
                    identity: block.identity,
                    byte: block.plain.len(),
                });
                captured = state.anchor.is_some();
            }
            let ranges = state
                .anchor
                .zip(state.end)
                .and_then(|(anchor, end)| read.ranges(anchor, end));
            if ranges.is_none() {
                state.anchor = None;
                state.end = None;
                state.dragging = false;
                state.link = None;
            }
            if command.is_some_and(|key| key.eq_ignore_ascii_case("c")) {
                if let Some(ranges) = &ranges {
                    let text = read.copy(ranges);
                    if !text.is_empty() {
                        clipboard.write(iced::advanced::clipboard::Kind::Standard, text);
                        captured = true;
                    }
                }
            }
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut Apply {
                    ranges: ranges.as_deref().unwrap_or(&[]),
                    index: 0,
                    changed: 0,
                    exchanges: Vec::new(),
                },
            );
        }
        // Let changed prose invalidate its retained exchange through its own
        // shell before consuming the event at the transcript boundary.
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
        if captured {
            shell.capture_event();
        }
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
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn surface<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
        Element::new(Selection {
            content: content.into(),
            context: None,
        })
    }
    fn read() -> Read {
        let mut read = Read::new(None);
        read.blocks = ["α first", "middle\ncode", "β last"]
            .into_iter()
            .enumerate()
            .map(|(index, plain)| Block {
                identity: index as u64 + 1,
                plain: Arc::from(plain),
                bounds: Rectangle::default(),
                visible: Some(Rectangle::default()),
                hit: 0,
                link: None,
            })
            .collect();
        read
    }
    #[test]
    fn forward_and_reverse_drag_copy_partial_endpoints_and_entire_middle_blocks() {
        let read = read();
        let start = Endpoint {
            identity: 1,
            byte: "α ".len(),
        };
        let end = Endpoint {
            identity: 3,
            byte: "β".len(),
        };
        for (anchor, end) in [(start, end), (end, start)] {
            let ranges = read.ranges(anchor, end).unwrap();
            assert_eq!(read.copy(&ranges), "first\n\nmiddle\ncode\n\nβ");
        }
    }
    #[test]
    fn removed_or_shortened_endpoints_cancel_without_slicing_invalid_unicode() {
        let read = read();
        let end = Endpoint {
            identity: 3,
            byte: 2,
        };
        for anchor in [
            Endpoint {
                identity: 4,
                byte: 0,
            },
            Endpoint {
                identity: 1,
                byte: 99,
            },
            Endpoint {
                identity: 1,
                byte: 1,
            },
        ] {
            assert!(read.ranges(anchor, end).is_none());
        }
    }
    #[test]
    fn drag_in_whitespace_reaches_nearest_block_and_reverse_drag_can_leave_viewport() {
        let mut read = read();
        for (index, block) in read.blocks.iter_mut().enumerate() {
            block.bounds = Rectangle {
                x: 20.0,
                y: index as f32 * 60.0,
                width: 200.0,
                height: 30.0,
            };
            block.visible = Some(block.bounds);
        }
        read.pointer = Some(Point::new(30.0, 45.0));
        assert!(read.endpoint(true).is_none());
        assert_eq!(read.endpoint(false).unwrap().identity, 1);
        read.pointer = Some(Point::new(30.0, -100.0));
        assert_eq!(read.endpoint(false).unwrap().identity, 1);
        read.pointer = Some(Point::new(30.0, 300.0));
        assert_eq!(read.endpoint(false).unwrap().identity, 3);
    }

    #[test]
    fn changing_conversations_clears_selection_even_when_prose_is_identical() {
        let renderer = crate::app::tests::headless_renderer();
        let first = Target {
            card: CardId::new(),
            generation: 0,
        };
        let second = Target {
            card: CardId::new(),
            generation: 0,
        };
        let mut element = transcript(prose("identical response"), first, None);
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let block = &read.blocks[0];
        let state = tree.state.downcast_mut::<State>();
        state.anchor = Some(Endpoint {
            identity: block.identity,
            byte: 0,
        });
        state.end = Some(Endpoint {
            identity: block.identity,
            byte: block.plain.len(),
        });
        state.focused = true;
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut replacement = transcript(prose("identical response"), second, None);
        tree.diff(&mut replacement);
        replacement
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let state = tree.state.downcast_ref::<State>();
        assert!(state.anchor.is_none());
        assert!(!state.focused);
        let prose = tree.children[0].state.downcast_ref::<Prose>();
        assert_eq!((prose.anchor, prose.end), (0, 0));
    }

    #[derive(Default)]
    struct ClipboardText(String);
    impl Clipboard for ClipboardText {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            Some(self.0.clone())
        }
        fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
            self.0 = contents;
        }
    }
    fn command(key: &str) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(key.into()),
            modified_key: keyboard::Key::Character(key.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        })
    }
    fn prose(text: &str) -> Element<'static, Message> {
        super::super::selectable_prose::text(
            vec![iced::widget::text::Span::new(text.to_owned())].into(),
            16.0.into(),
            iced::Font::default(),
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
        )
    }

    #[test]
    fn widget_drag_updates_retained_exchanges_and_copy_respects_focus_operations() {
        let renderer = crate::app::tests::headless_renderer();
        let mut element = surface(
            widget::column::with_children(vec![
                retained::kept(1, || prose("α first")),
                retained::kept(2, || prose("middle\ncode")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let viewport = Rectangle::with_size(Size::new(400.0, 400.0));
        let mut clipboard = ClipboardText::default();
        let mut read = Read::new(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        let first = read.blocks[0].bounds;
        let last = read.blocks[2].bounds;
        let start = Point::new(first.x, first.y + 3.0);
        let end = Point::new(last.x + last.width + 10.0, last.y + last.height + 1.0);
        for (event, point) in [
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                start,
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                end,
            ),
            (command("c"), end),
        ] {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert!(shell.is_event_captured());
            if shell.is_layout_invalid() {
                node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
            }
        }
        assert_eq!(clipboard.0, "α first\n\nmiddle\ncode\n\nβ last");
        // Reused retained layouts must have rebuilt their highlighted spans.
        fn highlights(tree: &Tree, count: &mut usize) {
            for child in &tree.children {
                if child.tag == tree::Tag::of::<Prose>() {
                    let prose = child.state.downcast_ref::<Prose>();
                    assert_eq!((prose.anchor, prose.end), (0, prose.plain.len()));
                    assert!(!prose.dirty);
                    *count += 1;
                }
                highlights(child, count);
            }
        }
        let mut count = 0;
        highlights(&tree, &mut count);
        assert_eq!(count, 3);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus(Id::new("chat-find")),
        );
        clipboard.0 = "find query".into();
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &command("c"),
            Layout::new(&node),
            mouse::Cursor::Available(end),
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(clipboard.0, "find query");
        assert!(!shell.is_event_captured());

        // A selected middle block changes while the endpoints survive.
        element = surface(
            widget::column::with_children(vec![
                retained::kept(1, || prose("α first")),
                retained::kept(4, || prose("new middle block")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut count = 0;
        highlights(&tree, &mut count);
        assert_eq!(
            count, 3,
            "layout reconciles all selected blocks before another input event"
        );

        // Paging replaces an endpoint in the same positional widget tree.
        element = surface(
            widget::column::with_children(vec![
                retained::kept(5, || prose("different first")),
                retained::kept(4, || prose("new middle block")),
                retained::kept(3, || prose("β last")),
            ])
            .spacing(20),
        );
        tree.diff(element.as_widget_mut());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(tree.state.downcast_ref::<State>().anchor.is_none());
        fn cleared(tree: &Tree) {
            for child in &tree.children {
                if child.tag == tree::Tag::of::<Prose>() {
                    let prose = child.state.downcast_ref::<Prose>();
                    assert_eq!((prose.anchor, prose.end), (0, 0));
                }
                cleared(child);
            }
        }
        cleared(&tree);
    }

    #[test]
    fn nested_scroll_translation_and_clipping_exclude_hidden_code_from_clicks() {
        let renderer = crate::app::tests::headless_renderer();
        let id = Id::new("code");
        let mut element: Element<'_, Message> = widget::scrollable(
            widget::container(
                widget::scrollable(prose(&"long code ".repeat(30)))
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        design::scrollbar(),
                    ))
                    .id(id.clone())
                    .width(120),
            )
            .padding(20),
        )
        .height(100)
        .width(300)
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to(
                id,
                operation::scrollable::AbsoluteOffset {
                    x: Some(60.0),
                    y: None,
                },
            ),
        );
        let mut read = Read::new(Some(Point::new(150.0, 25.0)));
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert_eq!(read.blocks.len(), 1);
        assert!(read.blocks[0].bounds.x < 20.0);
        assert!(read.blocks[0].bounds.contains(read.pointer.unwrap()));
        assert!(
            read.endpoint(true).is_none(),
            "text outside the code viewport is clipped"
        );
        read.pointer = Some(Point::new(30.0, 25.0));
        assert!(read.endpoint(true).is_some());
    }
}
