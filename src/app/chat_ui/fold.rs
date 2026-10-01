//! Content shown up to a height, with a control that shows the rest.
//!
//! Whether there is a rest to show is a question about the content as drawn:
//! Markdown joins a paragraph's lines and wraps them again at the width it is
//! given, so neither its line count nor its length says how tall it will be.
//! The fold lays the content out and decides from that, offering its control
//! only when the content does not fit.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{
    Background, Border, Color, Event, Gradient, Length, Point, Radians, Rectangle, Shadow, Size,
    Vector, gradient::Linear, mouse,
};
use cosmic::{Element, Renderer, Theme};

/// How tall a fold may be before it folds. Content up to a third taller is
/// shown whole: folding away a line or two saves the reader nothing.
const SLACK: f32 = 1.0 / 3.0;

/// How far above its edge folded content starts fading into `fill`.
const FADE: f32 = 32.0;

/// `content`, cut to `height` while `folded` if it is taller, with `toggle`
/// below it. `toggle` is laid out and drawn only when the content is tall
/// enough to fold, whether or not it is folded now, so the reader can fold
/// it again. `fill` is the colour behind the fold, which the cut fades into.
pub(super) fn fold<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    toggle: impl Into<Element<'a, Message>>,
    folded: bool,
    height: f32,
    spacing: f32,
    fill: fn(&Theme) -> Color,
) -> Element<'a, Message> {
    Element::new(Fold {
        content: content.into(),
        toggle: toggle.into(),
        folded,
        height,
        spacing,
        fill,
    })
}

struct Fold<'a, Message> {
    content: Element<'a, Message>,
    toggle: Element<'a, Message>,
    folded: bool,
    height: f32,
    spacing: f32,
    fill: fn(&Theme) -> Color,
}

/// What the last layout found, which drawing and input follow.
#[derive(Default)]
struct State {
    /// The content is tall enough to fold.
    tall: bool,
    /// The height of the content that is shown.
    shown: f32,
}

impl<Message> Fold<'_, Message> {
    /// Where the shown part of the content is, so input under the cut part
    /// does not reach text the reader cannot see.
    fn shown(state: &State, layout: Layout<'_>) -> Rectangle {
        let bounds = layout.bounds();
        Rectangle {
            height: state.shown,
            ..bounds
        }
    }

    fn cursor(state: &State, layout: Layout<'_>, cursor: mouse::Cursor) -> mouse::Cursor {
        match cursor.position() {
            Some(position) if !Self::shown(state, layout).contains(position) => {
                mouse::Cursor::Unavailable
            }
            _ => cursor,
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Fold<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content), Tree::new(&self.toggle)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [&mut self.content, &mut self.toggle]);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let width = limits.max().width;
        let unbounded = layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY));
        let content =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &unbounded);
        let full = content.size().height;
        let tall = full > self.height * (1.0 + SLACK);
        let shown = if tall && self.folded {
            self.height
        } else {
            full
        };
        let (toggle, total) = if tall {
            let toggle = self
                .toggle
                .as_widget_mut()
                .layout(&mut tree.children[1], renderer, &unbounded)
                .move_to(Point::new(0.0, shown + self.spacing));
            let total = shown + self.spacing + toggle.size().height;
            (toggle, total)
        } else {
            (layout::Node::new(Size::ZERO), shown)
        };
        *tree.state.downcast_mut::<State>() = State { tall, shown };
        layout::Node::with_children(
            limits.resolve(Length::Fill, Length::Shrink, Size::new(width, total)),
            vec![content, toggle],
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], content, renderer, operation);
        if tree.state.downcast_ref::<State>().tall {
            self.toggle
                .as_widget_mut()
                .operate(&mut tree.children[1], toggle, renderer, operation);
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
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let state = tree.state.downcast_ref::<State>();
        let tall = state.tall;
        let inner = Self::cursor(state, layout, cursor);
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            content,
            inner,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if tall {
            self.toggle.as_widget_mut().update(
                &mut tree.children[1],
                event,
                toggle,
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
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return mouse::Interaction::None;
        };
        let state = tree.state.downcast_ref::<State>();
        let shown = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            content,
            Self::cursor(state, layout, cursor),
            viewport,
            renderer,
        );
        if shown != mouse::Interaction::None || !state.tall {
            return shown;
        }
        self.toggle.as_widget().mouse_interaction(
            &tree.children[1],
            toggle,
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
        use cosmic::iced::advanced::Renderer as _;
        let mut children = layout.children();
        let (Some(content), Some(toggle)) = (children.next(), children.next()) else {
            return;
        };
        let state = tree.state.downcast_ref::<State>();
        let shown = Self::shown(state, layout);
        let cut = shown.height < content.bounds().height;
        let inner = Self::cursor(state, layout, cursor);
        if !cut {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                content,
                inner,
                viewport,
            );
        } else if let Some(clip) = shown.intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    content,
                    inner,
                    &clip,
                );
            });
            // A layer draws its quads beneath its text, so the fade over the
            // cut text is a layer of its own.
            renderer.with_layer(clip, |renderer| {
                let fill = (self.fill)(theme);
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            y: shown.y + shown.height - FADE,
                            height: FADE,
                            ..shown
                        },
                        border: Border::default(),
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    // π runs the gradient from top to bottom.
                    Background::Gradient(Gradient::Linear(
                        Linear::new(Radians(std::f32::consts::PI))
                            .add_stop(0.0, Color { a: 0.0, ..fill })
                            .add_stop(1.0, fill),
                    )),
                );
            });
        }
        if state.tall {
            self.toggle.as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                toggle,
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let content = layout.children().next()?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            content,
            renderer,
            viewport,
            translation,
        )
    }
}
