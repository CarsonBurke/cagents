//! Layout-time anchoring around iced's native scrollable. Its own state remains
//! authoritative for wheel, thumb, keyboard and touch input.
use super::view::Scroll;
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Event, Rectangle, Size, Vector};

/// `anchors` names direct children of the transcript column. Only their bounds
/// and stable identities are retained; the transcript's layout tree is not.
pub(super) fn transcript<'a>(
    content: impl Into<Element<'a, Message>>,
    target: Target,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
) -> Element<'a, Message> {
    Element::new(Anchored {
        content: content.into(),
        target: Some(target),
        subagent,
        anchors,
    })
}

/// Tool output follows its live end while preserving an explicitly scrolled
/// reading position. It does not participate in transcript pagination.
pub(super) fn tool_output<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Anchored {
        content: content.into(),
        target: None,
        subagent: None,
        anchors: Vec::new(),
    })
}

struct Anchored<'a> {
    target: Option<Target>,
    content: Element<'a, Message>,
    subagent: Option<&'a str>,
    anchors: Vec<(usize, &'a str)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    viewport: Size,
    content: Size,
}

impl Geometry {
    fn travel(self) -> f32 {
        (self.content.height - self.viewport.height).max(0.0)
    }

    fn node(self) -> layout::Node {
        layout::Node::with_children(self.viewport, vec![layout::Node::new(self.content)])
    }
}

struct Row {
    id: String,
    bounds: Rectangle,
}

#[derive(Default)]
struct Context {
    subagent: Option<String>,
    geometry: Option<Geometry>,
    rows: Vec<Row>,
    top: Option<f32>,
    used: u64,
}

impl Context {
    fn position(&self, next: Geometry, rows: &[(&str, Rectangle)]) -> f32 {
        let Some((previous, top)) = self.geometry.zip(self.top) else {
            return if self.subagent.is_some() {
                0.0
            } else {
                next.travel()
            };
        };
        if previous.travel() - top <= 0.5 && (previous.travel() > 0.0 || self.subagent.is_none()) {
            return next.travel();
        }
        if self.rows.is_empty() && rows.is_empty() {
            return top.clamp(0.0, next.travel());
        }
        let next_rows: std::collections::HashMap<_, _> = rows.iter().copied().collect();
        // Preserve the first visible surviving exchange, not the transcript's
        // total height: a page may arrive while the live answer also grows.
        let visible = self.rows.iter().filter(|row| {
            row.bounds.y + row.bounds.height > top && row.bounds.y < top + previous.viewport.height
        });
        let overlap = visible
            .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
            .next()
            .or_else(|| {
                // Front eviction can remove every visible exchange. Keep the
                // nearest surviving boundary instead of jumping to live output.
                self.rows
                    .iter()
                    .filter_map(|row| next_rows.get(row.id.as_str()).map(|bounds| (row, bounds)))
                    .min_by(|(a, _), (b, _)| {
                        (a.bounds.y - top)
                            .abs()
                            .total_cmp(&(b.bounds.y - top).abs())
                    })
            });
        overlap.map_or(next.travel(), |(old, new)| {
            let within = top - old.bounds.y;
            // A disclosure or reflow may shorten the anchored exchange itself.
            // Do not carry an offset past its new end into an unrelated turn.
            let within = if within > 0.0 {
                within.min(new.height.max(0.0))
            } else {
                within
            };
            (new.y + within).clamp(0.0, next.travel())
        })
    }

    fn remember(&mut self, next: Geometry, rows: &[(&str, Rectangle)], top: f32) {
        for (index, (id, bounds)) in rows.iter().enumerate() {
            if let Some(row) = self.rows.get_mut(index) {
                if row.id != *id {
                    row.id = (*id).to_owned();
                }
                row.bounds = *bounds;
            } else {
                self.rows.push(Row {
                    id: (*id).to_owned(),
                    bounds: *bounds,
                });
            }
        }
        self.rows.truncate(rows.len());
        self.geometry = Some(next);
        self.top = Some(top);
    }
}

#[derive(PartialEq)]
struct Report {
    subagent: Option<String>,
    top: f32,
    geometry: Geometry,
}

#[derive(Default)]
struct Anchor {
    contexts: Vec<Context>,
    active: Option<usize>,
    clock: u64,
    reported: Option<Report>,
    pending_input: bool,
}

impl Anchor {
    fn geometry(&self) -> Option<Geometry> {
        self.active.and_then(|index| self.contexts[index].geometry)
    }

    fn layout(
        &mut self,
        subagent: Option<&str>,
        rows: &[(&str, Rectangle)],
        next: Geometry,
        current_top: Option<f32>,
    ) -> Option<f32> {
        if let Some(index) = self.active
            && let Some(top) = current_top
        {
            self.contexts[index].top = Some(top);
        }
        let previous_active = self.active;
        let index = self
            .contexts
            .iter()
            .position(|context| context.subagent.as_deref() == subagent)
            .unwrap_or_else(|| {
                // Main is never evicted. Bound retained subagent positions even
                // when a long-lived session launches hundreds of workers.
                if self
                    .contexts
                    .iter()
                    .filter(|context| context.subagent.is_some())
                    .count()
                    >= 32
                {
                    let oldest = self
                        .contexts
                        .iter()
                        .enumerate()
                        .filter(|(_, context)| context.subagent.is_some())
                        .min_by_key(|(_, context)| context.used)
                        .unwrap()
                        .0;
                    self.contexts[oldest] = Context {
                        subagent: subagent.map(str::to_owned),
                        ..Context::default()
                    };
                    oldest
                } else {
                    self.contexts.push(Context {
                        subagent: subagent.map(str::to_owned),
                        ..Context::default()
                    });
                    self.contexts.len() - 1
                }
            });
        self.clock += 1;
        let context = &mut self.contexts[index];
        let switched = previous_active != Some(index) || context.geometry.is_none();
        let old_bottom = context
            .geometry
            .zip(context.top)
            .map(|(geometry, top)| (geometry.travel() - top).max(0.0));
        // Relative offsets (notably Home's Relative(1)) must become absolute
        // after reflow even when the desired bottom distance is unchanged.
        let freeze_reading = old_bottom.is_some_and(|bottom| bottom > 0.5)
            && context.geometry.is_some_and(|previous| previous != next);
        let top = context.position(next, rows);
        let bottom = next.travel() - top;
        context.used = self.clock;
        context.remember(next, rows, top);
        self.active = Some(index);
        if switched {
            self.pending_input = false;
        }
        (switched || freeze_reading || old_bottom.is_none_or(|old| (old - bottom).abs() > 0.5))
            .then_some(bottom)
    }
}

#[derive(Default)]
struct ReadTop(Option<f32>);

impl Operation for ReadTop {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.0 = Some(translation.y);
    }
}

struct SetBottom(f32);

impl Operation for SetBottom {
    fn traverse(&mut self, _: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
            x: None,
            y: Some(self.0),
        });
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Anchored<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Anchor>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Anchor::default())
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
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let anchor = tree.state.downcast_mut::<Anchor>();
        let child = &mut tree.children[0];
        let mut top = ReadTop::default();
        if let Some(previous) = anchor.geometry() {
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&previous.node()),
                renderer,
                &mut top,
            );
        }
        let node = self.content.as_widget_mut().layout(child, renderer, limits);
        let next = Geometry {
            viewport: node.size(),
            content: node.children()[0].size(),
        };
        let rows: Vec<_> = self
            .anchors
            .iter()
            .filter_map(|(index, id)| {
                node.children()[0]
                    .children()
                    .get(*index)
                    .map(|child| (*id, child.bounds()))
            })
            .collect();
        if let Some(offset) = anchor.layout(self.subagent, &rows, next, top.0) {
            self.content.as_widget_mut().operate(
                child,
                Layout::new(&node),
                renderer,
                &mut SetBottom(offset),
            );
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
        let mut before = ReadTop::default();
        if self.target.is_some() {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut before,
            );
        }
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        if self.target.is_some() {
            let mut after = ReadTop::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut after,
            );
            if before.0 != after.0 {
                tree.state.downcast_mut::<Anchor>().pending_input = true;
            }
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
        let mut before = ReadTop::default();
        if self.target.is_some() {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut before,
            );
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
        if self.target.is_none() {
            return;
        }
        let mut top = ReadTop::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut top);
        if let Some(top) = top.0 {
            let geometry = Geometry {
                viewport: layout.bounds().size(),
                content: layout
                    .children()
                    .next()
                    .expect("transcript content")
                    .bounds()
                    .size(),
            };
            let anchor = tree.state.downcast_mut::<Anchor>();
            let changed = anchor.reported.as_ref().is_none_or(|previous| {
                previous.subagent.as_deref() != self.subagent
                    || previous.top != top
                    || previous.geometry != geometry
            });
            // A fresh gesture at a pinned edge is also a retry signal. A
            // failed request must not require moving away and back again.
            let wheel = matches!(event, Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { y, .. } | mouse::ScrollDelta::Pixels { y, .. }
            }) if *y != 0.0)
                && cursor.is_over(layout.bounds())
                && cursor.is_over(*viewport);
            let user_input = wheel || before.0 != Some(top) || anchor.pending_input;
            if changed || user_input {
                anchor.pending_input = false;
                anchor.reported = Some(Report {
                    subagent: self.subagent.map(str::to_owned),
                    top,
                    geometry,
                });
                if let Some(target) = self.target {
                    shell.publish(action(Action::Scrolled(
                        target,
                        self.subagent.map(str::to_owned),
                        Scroll::new(top, geometry.content.height, geometry.viewport.height),
                        user_input,
                    )));
                }
            }
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

    fn geometry(height: f32) -> Geometry {
        Geometry {
            viewport: Size::new(400.0, 400.0),
            content: Size::new(400.0, height),
        }
    }
    fn rows<'a>(entries: &[(&'a str, f32, f32)]) -> Vec<(&'a str, Rectangle)> {
        entries
            .iter()
            .map(|(id, y, height)| {
                (
                    *id,
                    Rectangle {
                        x: 0.0,
                        y: *y,
                        width: 400.0,
                        height: *height,
                    },
                )
            })
            .collect()
    }
    fn reading() -> Anchor {
        let mut anchor = Anchor::default();
        assert_eq!(
            anchor.layout(
                None,
                &rows(&[
                    ("a", 0.0, 500.0),
                    ("b", 500.0, 500.0),
                    ("c", 1000.0, 1000.0)
                ]),
                geometry(2000.0),
                None
            ),
            Some(0.0)
        );
        anchor
    }

    #[test]
    fn native_layout_reports_fitting_content_and_anchors_real_column_children() {
        use iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let element = |entries: Vec<(&'static str, f32)>| {
            let anchors = entries
                .iter()
                .enumerate()
                .map(|(index, (id, _))| (index, *id))
                .collect();
            let children: Vec<Element<'static, Message>> = entries
                .into_iter()
                .map(|(_, height)| {
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(height)
                        .into()
                })
                .collect();
            transcript(
                widget::scrollable(widget::column::with_children(children).width(Length::Fill))
                    .anchor_bottom()
                    .id(Id::new("anchor-native-test"))
                    .height(Length::Fill)
                    .width(Length::Fill),
                target,
                None,
                anchors,
            )
        };
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(400.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, bounds.size());
        let mut fitting = element(vec![("short", 100.0)]);
        let mut tree = Tree::new(fitting.as_widget());
        let node = fitting
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = Vec::new();
        let event = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        fitting.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(actual, None, scroll, false))]
            if *actual == target && *scroll == Scroll::new(0.0, 100.0, 400.0))
        );

        messages.clear();
        fitting.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.is_empty(),
            "unchanged geometry must not trigger automatic paging loops"
        );
        fitting.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(
                messages.as_slice(),
                [Message::ChatUi(Action::Scrolled(_, None, _, true))]
            ),
            "a wheel gesture at a pinned edge must permit retry"
        );

        let mut initial = element(vec![("a", 500.0), ("b", 500.0), ("c", 1000.0)]);
        let mut tree = Tree::new(initial.as_widget());
        let node = initial
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        initial.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(1000.0),
        );
        initial.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages.clear();
        let mut updated = element(vec![
            ("older", 300.0),
            ("a", 500.0),
            ("b", 500.0),
            ("c", 1200.0),
        ]);
        tree.diff(updated.as_widget_mut());
        let node = updated
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut measured = ReadTop::default();
        updated
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
        assert_eq!(measured.0, Some(900.0));
        messages.clear();
        updated.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, false))]
            if *scroll == Scroll::new(900.0, 2500.0, 400.0))
        );

        // Equal-height replacement changes the offset through layout, not
        // through a gesture. Treating it as input bounces older pages to live.
        let mut replaced = element(vec![("d", 500.0), ("e", 500.0), ("f", 1500.0)]);
        tree.diff(replaced.as_widget_mut());
        let node = replaced
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, false))]
            if *scroll == Scroll::new(2100.0, 2500.0, 400.0))
        );
        let mut home = operation::scrollable::snap_to::<()>(
            Id::new("anchor-native-test"),
            iced::widget::scrollable::RelativeOffset {
                x: None,
                y: Some(1.0),
            },
        );
        replaced
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut home);
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, true))]
            if *scroll == Scroll::new(0.0, 2500.0, 400.0)),
            "Home's external operation is input"
        );
        messages.clear();
        replaced.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(messages.is_empty(), "input signal must be consumed once");
    }

    #[test]
    fn simultaneous_prepend_and_streaming_append_preserve_visible_exchange() {
        let next = rows(&[
            ("older", 0.0, 300.0),
            ("a", 300.0, 500.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1200.0),
        ]);
        assert_eq!(
            reading().layout(None, &next, geometry(2500.0), Some(600.0)),
            Some(1200.0)
        );
    }

    #[test]
    fn prepend_at_home_freezes_the_previous_first_exchange() {
        let mut anchor = reading();
        let next = rows(&[
            ("older", 0.0, 300.0),
            ("a", 300.0, 500.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1000.0),
        ]);
        assert_eq!(
            anchor.layout(None, &next, geometry(2300.0), Some(0.0)),
            Some(1600.0)
        );
        assert_eq!(
            anchor.layout(None, &next, geometry(2300.0), Some(300.0)),
            None
        );
    }

    #[test]
    fn growth_above_and_front_eviction_preserve_surviving_rows() {
        let next = rows(&[
            ("a", 0.0, 800.0),
            ("b", 800.0, 500.0),
            ("c", 1300.0, 1000.0),
        ]);
        assert_eq!(
            reading().layout(None, &next, geometry(2300.0), Some(600.0)),
            Some(1000.0)
        );
        let next = rows(&[("b", 0.0, 500.0), ("c", 500.0, 1000.0)]);
        assert_eq!(
            reading().layout(None, &next, geometry(1500.0), Some(600.0)),
            Some(1000.0)
        );
        assert_eq!(
            reading().layout(None, &next, geometry(1500.0), Some(100.0)),
            Some(1100.0)
        );
    }

    #[test]
    fn viewport_resize_and_live_end_use_the_current_native_offset() {
        let next = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1200.0),
        ]);
        let mut resized = geometry(2200.0);
        resized.viewport = Size::new(300.0, 600.0);
        assert_eq!(
            reading().layout(None, &next, resized, Some(900.0)),
            Some(700.0)
        );
        assert_eq!(reading().layout(None, &next, resized, Some(1600.0)), None);
    }

    #[test]
    fn shortened_exchange_clamps_its_internal_offset() {
        let next = rows(&[("a", 0.0, 500.0), ("b", 500.0, 50.0), ("c", 550.0, 1000.0)]);
        assert_eq!(
            reading().layout(None, &next, geometry(1550.0), Some(800.0)),
            Some(600.0)
        );
    }

    #[test]
    fn separate_transcripts_restore_positions_and_have_deliberate_initial_edges() {
        let mut anchor = reading();
        let child = rows(&[("child", 0.0, 1600.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
            Some(1200.0)
        );
        let main = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1200.0),
        ]);
        assert_eq!(
            anchor.layout(None, &main, geometry(2200.0), Some(300.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
            Some(900.0)
        );
    }

    #[test]
    fn short_subagent_content_preserves_its_initial_top_when_it_grows() {
        let mut anchor = Anchor::default();
        let initial = rows(&[("child", 0.0, 100.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &initial, geometry(100.0), None),
            Some(0.0)
        );
        let grown = rows(&[("child", 0.0, 1600.0)]);
        assert_eq!(
            anchor.layout(Some("worker"), &grown, geometry(1600.0), Some(0.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(
                Some("worker"),
                &rows(&[("child", 0.0, 1800.0)]),
                geometry(1800.0),
                Some(1200.0)
            ),
            None,
            "End explicitly follows the live end after content becomes scrollable"
        );
    }

    #[test]
    fn bounded_contexts_keep_main_even_after_many_workers() {
        let mut anchor = reading();
        let child = rows(&[("child", 0.0, 1600.0)]);
        for i in 0..64 {
            anchor.layout(
                Some(&format!("worker-{i}")),
                &child,
                geometry(1600.0),
                Some(600.0),
            );
        }
        assert_eq!(anchor.contexts.len(), 33);
        let main = rows(&[
            ("a", 0.0, 500.0),
            ("b", 500.0, 500.0),
            ("c", 1000.0, 1000.0),
        ]);
        assert_eq!(
            anchor.layout(None, &main, geometry(2000.0), Some(300.0)),
            Some(1000.0)
        );
    }

    #[test]
    fn tool_output_without_message_anchors_preserves_reading_offset() {
        let mut anchor = Anchor::default();
        assert_eq!(anchor.layout(None, &[], geometry(2000.0), None), Some(0.0));
        assert_eq!(
            anchor.layout(None, &[], geometry(2200.0), Some(600.0)),
            Some(1200.0)
        );
        assert_eq!(
            anchor.layout(None, &[], geometry(2400.0), Some(1800.0)),
            None
        );
    }

    #[test]
    fn entirely_replaced_window_starts_at_its_newest_boundary() {
        assert_eq!(
            reading().layout(
                None,
                &rows(&[("older", 0.0, 900.0)]),
                geometry(900.0),
                Some(0.0)
            ),
            Some(0.0)
        );
    }
}
