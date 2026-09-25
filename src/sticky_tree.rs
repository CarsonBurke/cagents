//! A scrollable file tree whose folder rows pin above their descendants.
//!
//! The same row widget is used for drawing and input, so a pinned folder keeps
//! its native button focus, accessibility and click target. Groups are nested
//! by directory depth and only the currently visible rows are mounted.

use std::{cell::Cell, rc::Rc};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::{
    Event, Length, Point, Rectangle, Size, Vector,
    advanced::{
        Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
        widget::{Operation, Tree, tree},
    },
    mouse,
};
use cosmic::widget;
use cosmic::{Element, Renderer, Theme};

#[derive(Clone, Default)]
pub(crate) struct Viewport(Rc<Cell<f32>>);

pub(crate) struct Row<'a, Message> {
    pub depth: usize,
    pub directory: bool,
    pub element: Element<'a, Message>,
}

/// Build nested groups from display-order rows. A folder is the header for
/// exactly its descendants. Callers bound the rows they pass where needed.
pub(crate) fn rows<'a, Message: 'a>(
    rows: Vec<Row<'a, Message>>,
    row_height: f32,
) -> Element<'a, Message> {
    struct Frame<'a, Message> {
        depth: usize,
        header: Element<'a, Message>,
        children: Vec<Element<'a, Message>>,
    }
    let position = Viewport::default();
    let mut root = Vec::new();
    let mut folders: Vec<Frame<'a, Message>> = Vec::new();
    let close = |folders: &mut Vec<Frame<'a, Message>>, root: &mut Vec<Element<'a, Message>>| {
        let frame = folders.pop().expect("open folder");
        let descendants = widget::column::with_children(frame.children)
            .spacing(0)
            .into();
        let group = group(
            frame.header,
            descendants,
            frame.depth,
            row_height,
            position.clone(),
        );
        if let Some(parent) = folders.last_mut() {
            parent.children.push(group);
        } else {
            root.push(group);
        }
    };
    for row in rows {
        while folders
            .last()
            .is_some_and(|folder| folder.depth >= row.depth)
        {
            close(&mut folders, &mut root);
        }
        if row.directory {
            folders.push(Frame {
                depth: row.depth,
                header: row.element,
                children: Vec::new(),
            });
        } else if let Some(folder) = folders.last_mut() {
            folder.children.push(row.element);
        } else {
            root.push(row.element);
        }
    }
    while !folders.is_empty() {
        close(&mut folders, &mut root);
    }
    viewport(
        widget::column::with_children(root).spacing(0).into(),
        position,
    )
}

pub(crate) fn viewport<'a, Message: 'a>(
    child: Element<'a, Message>,
    position: Viewport,
) -> Element<'a, Message> {
    Element::new(StickyTree {
        children: vec![child],
        kind: Kind::Viewport(position),
    })
}

pub(crate) fn group<'a, Message: 'a>(
    header: Element<'a, Message>,
    descendants: Element<'a, Message>,
    depth: usize,
    header_height: f32,
    position: Viewport,
) -> Element<'a, Message> {
    Element::new(StickyTree {
        children: vec![header, descendants],
        kind: Kind::Group {
            position,
            offset: depth as f32 * header_height,
        },
    })
}

enum Kind {
    Viewport(Viewport),
    Group { position: Viewport, offset: f32 },
}

#[derive(Default)]
struct State {
    header: Option<layout::Node>,
}

struct StickyTree<'a, Message> {
    children: Vec<Element<'a, Message>>,
    kind: Kind,
}

fn pinned_bounds(group: Rectangle, height: f32, top: f32) -> Rectangle {
    Rectangle {
        y: top
            .max(group.y)
            .min(group.y + (group.height - height).max(0.0)),
        height,
        ..group
    }
}

fn child_layouts<'a>(
    kind: &Kind,
    state: &'a State,
    layout: Layout<'a>,
) -> impl DoubleEndedIterator<Item = Layout<'a>> + ExactSizeIterator + use<'a> {
    let top = match kind {
        Kind::Viewport(_) => None,
        Kind::Group { position, offset } => Some(position.0.get() + offset),
    };
    layout.children().enumerate().map(move |(index, child)| {
        let child = if index == 0
            && let Some(top) = top
        {
            let header = state.header.as_ref().expect("sticky group laid out");
            let bounds = pinned_bounds(layout.bounds(), header.size().height, top);
            Layout::with_offset(Vector::new(bounds.x, bounds.y), header)
        } else {
            child
        };
        child.with_virtual_offset(layout.virtual_offset())
    })
}

impl<Message> StickyTree<'_, Message> {
    fn refresh_position(&self, viewport: &Rectangle) {
        if let Kind::Viewport(position) = &self.kind {
            position.0.set(viewport.y);
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for StickyTree<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        if matches!(self.kind, Kind::Group { .. }) {
            Size::new(Length::Fill, Length::Shrink)
        } else {
            self.children[0].as_widget().size()
        }
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let header =
            self.children[0]
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);
        if matches!(self.kind, Kind::Viewport(_)) {
            return layout::Node::with_children(header.size(), vec![header]);
        }
        let size = header.size();
        let descendants = self.children[1]
            .as_widget_mut()
            .layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(size.width, limits.max().height)),
            )
            .move_to(Point::new(0.0, size.height));
        let total = Size::new(
            size.width.max(descendants.size().width),
            size.height + descendants.size().height,
        );
        tree.state.downcast_mut::<State>().header = Some(header);
        layout::Node::with_children(total, vec![layout::Node::new(size), descendants])
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Kind::Viewport(position) = &self.kind {
            position
                .0
                .set(layout.bounds().y + layout.virtual_offset().y);
        }
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, child_tree), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(child_layouts(
                    &self.kind,
                    tree.state.downcast_ref::<State>(),
                    layout,
                ))
            {
                child
                    .as_widget_mut()
                    .operate(child_tree, child_layout, renderer, operation);
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
        self.refresh_position(viewport);
        let group = matches!(self.kind, Kind::Group { .. });
        let header = child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout)
            .next()
            .unwrap()
            .bounds();
        let descendants_viewport = if group {
            let top = viewport.y.max(header.y + header.height);
            Rectangle {
                y: top,
                height: (viewport.y + viewport.height - top).max(0.0),
                ..*viewport
            }
        } else {
            *viewport
        };
        for (index, ((child, child_tree), child_layout)) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(child_layouts(
                &self.kind,
                tree.state.downcast_ref::<State>(),
                layout,
            ))
            .enumerate()
            .rev()
        {
            let viewport = if group && index == 1 {
                &descendants_viewport
            } else {
                viewport
            };
            let cursor = if cursor.is_over(*viewport) {
                cursor
            } else {
                mouse::Cursor::Unavailable
            };
            child.as_widget_mut().update(
                child_tree,
                event,
                child_layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if shell.is_event_captured() {
                break;
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
        self.refresh_position(viewport);
        let header = child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout)
            .next()
            .unwrap()
            .bounds();
        self.children
            .iter()
            .zip(&tree.children)
            .zip(child_layouts(
                &self.kind,
                tree.state.downcast_ref::<State>(),
                layout,
            ))
            .enumerate()
            .rev()
            .map(|(index, ((child, child_tree), child_layout))| {
                let covered =
                    index == 1 && matches!(self.kind, Kind::Group { .. }) && cursor.is_over(header);
                let cursor = if covered || !cursor.is_over(*viewport) {
                    mouse::Cursor::Unavailable
                } else {
                    cursor
                };
                child.as_widget().mouse_interaction(
                    child_tree,
                    child_layout,
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .find(|interaction| *interaction != mouse::Interaction::None)
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
        self.refresh_position(viewport);
        if matches!(self.kind, Kind::Group { .. }) {
            let mut layouts = child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout);
            let header = layouts.next().unwrap();
            let descendants = layouts.next().unwrap();
            self.children[1].as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                descendants,
                cursor,
                viewport,
            );
            if let Some(clip) = header.bounds().intersection(viewport) {
                renderer.with_layer(clip, |renderer| {
                    if header.bounds().y > layout.bounds().y {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: header.bounds(),
                                ..Default::default()
                            },
                            theme.current_container().base,
                        );
                    }
                    self.children[0].as_widget().draw(
                        &tree.children[0],
                        renderer,
                        theme,
                        style,
                        header,
                        cursor,
                        viewport,
                    );
                });
            }
        } else {
            let child_layout =
                child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout)
                    .next()
                    .unwrap();
            self.children[0].as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
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
        if let Kind::Viewport(position) = &self.kind {
            position.0.set(viewport.y - translation.y);
        }
        let overlays = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(child_layouts(
                &self.kind,
                tree.state.downcast_ref::<State>(),
                layout,
            ))
            .filter_map(|((child, child_tree), child_layout)| {
                child.as_widget_mut().overlay(
                    child_tree,
                    child_layout,
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect::<Vec<_>>();
        (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let mut accessible = iced_accessibility::A11yTree::join(
            self.children
                .iter()
                .zip(&tree.children)
                .zip(child_layouts(
                    &self.kind,
                    tree.state.downcast_ref::<State>(),
                    layout,
                ))
                .map(|((child, child_tree), child_layout)| {
                    child
                        .as_widget()
                        .a11y_nodes(child_layout, child_tree, cursor)
                }),
        );
        if matches!(self.kind, Kind::Viewport(_)) {
            // Native buttons report content coordinates. Translate at the
            // single list boundary so pinned headers have their visual bounds.
            let offset = layout.virtual_offset();
            let translate = |node: &mut iced_accessibility::A11yNode| {
                if let Some(bounds) = node.node().bounds() {
                    node.node_mut()
                        .set_bounds(iced_accessibility::accesskit::Rect::new(
                            bounds.x0 - f64::from(offset.x),
                            bounds.y0 - f64::from(offset.y),
                            bounds.x1 - f64::from(offset.x),
                            bounds.y1 - f64::from(offset.y),
                        ));
                }
            };
            accessible.root_mut().iter_mut().for_each(translate);
            accessible.children_mut().iter_mut().for_each(translate);
        }
        accessible
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, child_tree), child_layout) in self.children.iter().zip(&tree.children).zip(
            child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout),
        ) {
            child
                .as_widget()
                .drag_destinations(child_tree, child_layout, renderer, destinations);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_header_pins_within_its_own_descendant_group() {
        let group = Rectangle::new(Point::new(12.0, 50.0), Size::new(220.0, 180.0));
        let before = pinned_bounds(group, 24.0, 20.0);
        assert_eq!(before.y, 50.0);
        assert_eq!(pinned_bounds(group, 24.0, 90.0).y, 90.0);
        // At the final child, the folder slides out as the next sibling enters.
        assert_eq!(pinned_bounds(group, 24.0, 250.0).y, 206.0);
        assert_eq!(pinned_bounds(group, 24.0, 90.0).x, 12.0);
    }
}
