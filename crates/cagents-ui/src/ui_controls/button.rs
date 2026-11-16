//! The shell's labelled buttons.
//!
//! A command with a name is a compact, squared control at the shell's own
//! type size, in one of four weights. Stock COSMIC buttons are tall pills at
//! the desktop's body size; beside the workbench's 13px rows they read as a
//! second application, and every dialog that mixed the two had no single
//! control height to align to.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Alignment, Length},
    widget,
};

use crate::design::{self, space};

/// How much of the surface's attention a button asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    /// The one action a surface exists to offer.
    Primary,
    /// An ordinary command.
    Secondary,
    /// The action that discards or deletes.
    Destructive,
    /// A command that should stay out of the way until it is wanted.
    Quiet,
}

pub struct Button<'a, Message> {
    /// Every label the button can show, the one it was made with first. It
    /// keeps the room of the widest, whichever it shows.
    labels: Vec<Cow<'a, str>>,
    /// The index in `labels` of the one it shows.
    shown: usize,
    weight: Weight,
    name: Option<Cow<'a, str>>,
    icon: Option<widget::icon::Handle>,
    on_press: Option<Message>,
    width: Length,
    id: Option<widget::Id>,
}

/// The one action a surface exists to offer. At most one per surface.
pub fn primary<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Primary)
}

/// An ordinary command.
pub fn secondary<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Secondary)
}

/// The action that discards or deletes something.
pub fn destructive<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Destructive)
}

/// A command without a surface of its own until it is hovered.
pub fn quiet<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Quiet)
}

impl<'a, Message> Button<'a, Message> {
    fn new(label: impl Into<Cow<'a, str>>, weight: Weight) -> Self {
        Self {
            labels: vec![label.into()],
            shown: 0,
            weight,
            name: None,
            icon: None,
            on_press: None,
            width: Length::Shrink,
            id: None,
        }
    }

    /// A button without a press message is drawn and announced as disabled.
    pub fn on_press(mut self, message: Message) -> Self {
        self.on_press = Some(message);
        self
    }

    pub fn on_press_maybe(mut self, message: Option<Message>) -> Self {
        self.on_press = message;
        self
    }

    /// The accessible name, where the visible label alone would not say what
    /// the button acts on. Defaults to the label it shows.
    pub fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// A glyph ahead of the label.
    pub fn icon(mut self, icon: impl Into<widget::icon::Handle>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// Shows `label` in place of the one the button was made with while
    /// `shown`: a busy state such as "Saving…", an outcome such as "Retry",
    /// or the other side of a toggle. Give it on every render, shown or not:
    /// the button always keeps the room of the widest label it was given, so
    /// neither it nor the controls beside it move when its label changes.
    /// Where several are shown, the first wins, as in an `if`/`else if`.
    ///
    /// Labels it keeps room for are measured with the visible label's font,
    /// but never drawn or announced.
    pub fn instead(mut self, label: impl Into<Cow<'a, str>>, shown: bool) -> Self {
        if shown && self.shown == 0 {
            self.shown = self.labels.len();
        }
        self.labels.push(label.into());
        self
    }

    /// Keeps room for labels shown in this button's place when the choice is
    /// made elsewhere: a different button swapped into its slot, or a label
    /// chosen by a model the view only reads. Where the view picks the
    /// label, [`Self::instead`] says so in one place.
    pub fn keep_room_for(
        mut self,
        labels: impl IntoIterator<Item = impl Into<Cow<'a, str>>>,
    ) -> Self {
        self.labels.extend(labels.into_iter().map(Into::into));
        self
    }

    /// A button wider than its label centers the label.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn id(mut self, id: widget::Id) -> Self {
        self.id = Some(id);
        self
    }
}

impl<'a, Message: Clone + 'a> From<Button<'a, Message>> for Element<'a, Message> {
    fn from(button: Button<'a, Message>) -> Self {
        let Button {
            mut labels,
            shown,
            weight,
            name,
            icon,
            on_press,
            width,
            id,
        } = button;
        let mut content = widget::row::with_capacity(2)
            .spacing(space::XS + space::XXS)
            .align_y(Alignment::Center);
        // A glyph and its label read as one: the label stays beside it, and
        // the room kept for a longer label trails it.
        let label_align = if icon.is_some() {
            iced::alignment::Horizontal::Left
        } else {
            iced::alignment::Horizontal::Center
        };
        if let Some(icon) = icon {
            content = content.push(widget::icon(icon).size(design::ROW_ICON_SIZE));
        }
        let heading = |label: Cow<'a, str>| -> Element<'a, Message> {
            design::heading(label)
                .wrapping(iced::widget::text::Wrapping::None)
                .into()
        };
        let label = labels.swap_remove(shown);
        content = content.push(if labels.is_empty() {
            heading(label.clone())
        } else {
            super::reserved(
                heading(label.clone()),
                labels.into_iter().map(heading),
                label_align,
            )
        });
        let mut button = widget::button::custom(
            widget::container(content)
                // COSMIC's transparent container resets the inherited ink.
                // This wrapper only aligns, so it must pass the button's
                // status colors through to the label and glyph.
                .class(design::transparent())
                .center_x(width)
                .center_y(Length::Fill),
        )
        .name(name.unwrap_or(label))
        .class(style(weight))
        .padding([0.0, space::M])
        .width(width)
        .height(design::DENSE_ROW_HEIGHT)
        .on_press_maybe(on_press);
        if let Some(id) = id {
            button = button.id(id);
        }
        button.into()
    }
}

/// The fill, edge and ink of one button weight in one interaction state.
struct Paint {
    fill: Option<iced::Color>,
    edge: Option<iced::Color>,
    ink: iced::Color,
}

#[derive(Clone, Copy)]
enum Status {
    Active,
    Hovered,
    Pressed,
    Disabled,
}

fn paint(weight: Weight, status: Status, theme: &cosmic::Theme) -> Paint {
    let cosmic = theme.cosmic();
    let container = theme.current_container();
    let surface: iced::Color = container.base.into();
    let on: iced::Color = container.on.into();
    // How far a neutral fill lifts off the surface in each state.
    let lift = |amount| design::mix(surface, on, amount);
    if matches!(status, Status::Disabled) {
        return Paint {
            fill: (weight != Weight::Quiet).then(|| lift(0.05)),
            edge: None,
            ink: design::disabled_color(theme),
        };
    }
    let filled = |component: &cosmic::cosmic_theme::Component| Paint {
        fill: Some(
            match status {
                Status::Hovered => component.hover,
                Status::Pressed => component.pressed,
                Status::Active | Status::Disabled => component.base,
            }
            .into(),
        ),
        edge: None,
        ink: component.on.into(),
    };
    match weight {
        Weight::Primary => filled(&cosmic.accent_button),
        Weight::Destructive => filled(&cosmic.destructive_button),
        Weight::Secondary => Paint {
            fill: Some(lift(match status {
                Status::Hovered => 0.11,
                Status::Pressed => 0.15,
                Status::Active | Status::Disabled => 0.07,
            })),
            edge: Some(design::hairline(theme)),
            ink: on,
        },
        Weight::Quiet => Paint {
            fill: match status {
                Status::Hovered => Some(lift(0.08)),
                Status::Pressed => Some(lift(0.12)),
                Status::Active | Status::Disabled => None,
            },
            edge: None,
            ink: on,
        },
    }
}

/// The quiet weight's look for a control with content of its own, such as
/// a checkbox row: no surface until hovered, ordinary ink and the shared
/// focus ring.
pub fn quiet_class() -> cosmic::theme::Button {
    style(Weight::Quiet)
}

fn style(weight: Weight) -> cosmic::theme::Button {
    let appearance = move |status: Status, focused: bool, theme: &cosmic::Theme| {
        let Paint { fill, edge, ink } = paint(weight, status, theme);
        widget::button::Style {
            background: fill.map(iced::Background::Color),
            border_radius: design::RADIUS_S.into(),
            border_width: if edge.is_some() {
                design::BORDER_WIDTH
            } else {
                0.0
            },
            border_color: edge.unwrap_or(iced::Color::TRANSPARENT),
            // Keyboard focus is a ring outside the control, so it reads the
            // same on a filled button as on a quiet one. Beside a bright
            // fill a hairline disappears, so a filled button's ring is
            // twice as wide: in a question that answers Enter, which choice
            // has focus is the thing to see.
            outline_width: match (focused, weight) {
                (false, _) => 0.0,
                (true, Weight::Primary | Weight::Destructive) => 2.0 * design::BORDER_WIDTH,
                (true, Weight::Secondary | Weight::Quiet) => design::BORDER_WIDTH,
            },
            outline_color: if focused {
                design::accent_color(theme)
            } else {
                iced::Color::TRANSPARENT
            },
            icon_color: Some(ink),
            text_color: Some(ink),
            ..design::button_style()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| appearance(Status::Active, focused, theme)),
        disabled: Box::new(move |theme| appearance(Status::Disabled, false, theme)),
        hovered: Box::new(move |focused, theme| appearance(Status::Hovered, focused, theme)),
        pressed: Box::new(move |focused, theme| appearance(Status::Pressed, focused, theme)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The button made with `labels[0]`, offering the rest `instead`, while
    /// it shows `current`.
    fn showing<'a>(labels: &[&'a str], current: &str) -> Button<'a, ()> {
        labels[1..]
            .iter()
            .fold(primary(labels[0]), |button, label| {
                button.instead(*label, *label == current)
            })
    }

    /// The bounds of a row's children, and the names it announces.
    fn laid_out_row(
        row: &mut Element<'_, ()>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        width: f32,
    ) -> (Vec<cosmic::iced::Rectangle>, Vec<String>) {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        tree.diff(row.as_widget_mut());
        let node = row.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 80.0)),
        );
        let bounds = node.children().iter().map(layout::Node::bounds).collect();
        let accessible =
            row.as_widget()
                .a11y_nodes(Layout::new(&node), tree, mouse::Cursor::Unavailable);
        let names = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect();
        (bounds, names)
    }

    /// Whatever label a button shows, it and the enabled control beside it
    /// keep their bounds, and only the label it shows is announced.
    #[test]
    fn changing_labels_keep_button_and_enabled_neighbor_bounds() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{layout, widget::Tree};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let cases: [&[&str]; 7] = [
            &["Create worktree", "Creating…"],
            &["Reconnect", "Checking…"],
            &["Sign in", "Signing in…"],
            &["Close", "Checking…"],
            &["End 12 processes and close", "Ending…"],
            &[
                "Load more viewed states",
                "Loading viewed states…",
                "Retry viewed states",
            ],
            // Width comes from shaping, not character count.
            &["WWW", "iiii"],
        ];
        for labels in cases {
            let widest = labels
                .iter()
                .map(|label| {
                    let mut plain: Element<'_, ()> = primary(*label).into();
                    let mut tree = Tree::new(plain.as_widget());
                    plain
                        .as_widget_mut()
                        .layout(
                            &mut tree,
                            &renderer,
                            &layout::Limits::new(Size::ZERO, Size::new(900.0, 80.0)),
                        )
                        .size()
                        .width
                })
                .fold(0.0, f32::max);
            for width in [320.0, 900.0] {
                let mut tree = Tree::empty();
                let mut first = None;
                for current in labels {
                    let mut row: Element<'_, ()> = widget::row::with_children(vec![
                        showing(labels, current).on_press(()).into(),
                        secondary("Cancel").on_press(()).into(),
                    ])
                    .spacing(space::S)
                    .into();
                    let (bounds, names) = laid_out_row(&mut row, &mut tree, width);
                    assert_eq!(
                        bounds[0].width, widest,
                        "{labels:?} must keep the widest label's room"
                    );
                    match &first {
                        Some(first) => assert_eq!(&bounds, first, "{labels:?} at {width}px"),
                        None => first = Some(bounds),
                    }
                    assert!(names.iter().any(|name| name == current), "{names:?}");
                    for hidden in labels.iter().filter(|label| **label != *current) {
                        assert!(
                            !names.iter().any(|name| name == hidden),
                            "{hidden} must not be announced while {current} shows: {names:?}"
                        );
                    }
                }
            }
        }
    }

    /// The first label shown wins, as in an `if`/`else if`; one kept for
    /// another button takes room but is never shown.
    #[test]
    fn the_first_shown_label_wins_and_kept_room_is_never_shown() {
        use cosmic::iced::advanced::widget::Tree;

        let names = |button: Button<'static, ()>| {
            let mut row: Element<'_, ()> = widget::row::with_children(vec![button.into()]).into();
            laid_out_row(&mut row, &mut Tree::empty(), 900.0).1
        };
        let both = primary("Load more")
            .instead("Loading…", true)
            .instead("Retry", true);
        assert!(names(both).iter().any(|name| name == "Loading…"));
        let neither = primary("Load more")
            .instead("Loading…", false)
            .instead("Retry", false);
        assert!(names(neither).iter().any(|name| name == "Load more"));
        let kept = secondary("Check again").keep_room_for(["Restart daemon"]);
        let shown = names(kept);
        assert!(shown.iter().any(|name| name == "Check again"), "{shown:?}");
        assert!(
            !shown.iter().any(|name| name == "Restart daemon"),
            "{shown:?}"
        );

        let mut tree = Tree::empty();
        let mut kept: Element<'_, ()> = widget::row::with_children(vec![
            secondary("Check again")
                .keep_room_for(["Restart daemon"])
                .into(),
        ])
        .into();
        let mut wide: Element<'_, ()> =
            widget::row::with_children(vec![destructive("Restart daemon").into()]).into();
        assert_eq!(
            laid_out_row(&mut kept, &mut tree, 900.0).0[0].width,
            laid_out_row(&mut wide, &mut Tree::empty(), 900.0).0[0].width,
            "the swapped-in button keeps the room of the one it replaces"
        );
    }

    /// A glyph and its label stay together whichever label shows: the room
    /// kept for a longer one trails the label rather than splitting them,
    /// as it would for a disclosure such as "Hide options".
    #[test]
    fn a_shorter_label_stays_beside_its_glyph() {
        use cosmic::iced::advanced::{layout, widget::Tree};
        use cosmic::iced::{Point, Rectangle, Size, Vector};

        fn leaves(node: &layout::Node, origin: Point, found: &mut Vec<Rectangle>) {
            let bounds = node.bounds() + Vector::new(origin.x, origin.y);
            if node.children().is_empty() {
                found.push(bounds);
            }
            for child in node.children() {
                leaves(child, bounds.position(), found);
            }
        }
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let gap = |advanced: bool| {
            let mut element: Element<'_, ()> = quiet("Base branch & destination")
                .instead("Hide options", advanced)
                .icon(widget::icon::from_name("pan-down-symbolic"))
                .on_press(())
                .into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(900.0, 80.0)),
            );
            let mut found = Vec::new();
            leaves(&node, Point::ORIGIN, &mut found);
            let [glyph, label] = found[..] else {
                panic!("expected a glyph and a label, found {found:?}");
            };
            label.x - (glyph.x + glyph.width)
        };
        assert_eq!(gap(true), gap(false));
        assert_eq!(gap(false), space::XS + space::XXS);
    }

    #[test]
    fn buttons_share_the_dense_control_height_and_disable_without_a_message() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 80.0));
        for (constructor, message) in [
            (primary as fn(&'static str) -> Button<'static, ()>, Some(())),
            (secondary, Some(())),
            (destructive, None),
            (quiet, Some(())),
        ] {
            let mut element: Element<'_, ()> = constructor("Apply").on_press_maybe(message).into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            assert_eq!(node.size().height, design::DENSE_ROW_HEIGHT);
            let cursor = mouse::Cursor::Available(node.bounds().center());
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert_eq!(messages.len(), usize::from(message.is_some()));
        }
    }

    /// Tab moves through the buttons that can act and passes a disabled one
    /// over. A button disabled while focused, such as Refresh while it runs,
    /// keeps the focus, and Tab moves on from it. Operations that locate
    /// buttons still find a disabled one.
    #[test]
    fn disabled_buttons_are_not_focus_stops() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::widget::{
            Operation, Tree,
            operation::{self, Outcome, focusable},
        };
        use cosmic::iced::advanced::{Layout, layout};
        use std::sync::{Arc, Mutex};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let ids = ["first", "disabled", "last"].map(widget::Id::new);
        let row = |enabled: [bool; 3]| -> Element<'static, u8> {
            widget::row::with_children(
                ids.iter()
                    .zip(enabled)
                    .enumerate()
                    .map(|(index, (id, enabled))| {
                        widget::button::custom(design::caption("Action"))
                            .id(id.clone())
                            .width(Length::Fixed(80.0))
                            .on_press_maybe(enabled.then_some(index as u8))
                            .into()
                    })
                    .collect::<Vec<_>>(),
            )
            .into()
        };
        let run = |element: &mut Element<'static, u8>,
                   tree: &mut Tree,
                   mut operation: Box<dyn Operation>| {
            let node = element.as_widget_mut().layout(
                tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
            );
            loop {
                element.as_widget_mut().operate(
                    tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
                match operation.finish() {
                    Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        };
        let focused = |element: &mut Element<'static, u8>, tree: &mut Tree| {
            let found: Arc<Mutex<Option<widget::Id>>> = Arc::default();
            let sink = Arc::clone(&found);
            run(
                element,
                tree,
                Box::new(operation::map(focusable::find_focused(), move |id| {
                    *sink.lock().unwrap() = Some(id);
                })),
            );
            found.lock().unwrap().clone()
        };

        let mut element = row([true, false, true]);
        let mut tree = Tree::new(&element);
        run(&mut element, &mut tree, Box::new(focusable::focus_next()));
        assert_eq!(focused(&mut element, &mut tree), Some(ids[0].clone()));
        run(&mut element, &mut tree, Box::new(focusable::focus_next()));
        assert_eq!(
            focused(&mut element, &mut tree),
            Some(ids[2].clone()),
            "Tab passes over the disabled button"
        );
        run(
            &mut element,
            &mut tree,
            Box::new(focusable::focus_previous()),
        );
        assert_eq!(
            focused(&mut element, &mut tree),
            Some(ids[0].clone()),
            "Shift+Tab passes over it too"
        );

        struct Locate(Arc<Mutex<Vec<widget::Id>>>);
        impl Operation for Locate {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: cosmic::iced::Rectangle,
                _: &mut dyn focusable::Focusable,
            ) {
                self.0.lock().unwrap().extend(id.cloned());
            }
        }
        let located: Arc<Mutex<Vec<widget::Id>>> = Arc::default();
        run(
            &mut element,
            &mut tree,
            Box::new(Locate(Arc::clone(&located))),
        );
        assert_eq!(
            *located.lock().unwrap(),
            ids.to_vec(),
            "operations still find the disabled button"
        );

        let mut element = row([true, true, true]);
        tree.diff(element.as_widget_mut());
        run(
            &mut element,
            &mut tree,
            Box::new(focusable::focus(ids[1].clone())),
        );
        let mut element = row([true, false, true]);
        tree.diff(element.as_widget_mut());
        assert_eq!(
            focused(&mut element, &mut tree),
            Some(ids[1].clone()),
            "a button disabled while focused keeps the focus"
        );
        run(&mut element, &mut tree, Box::new(focusable::focus_next()));
        assert_eq!(
            focused(&mut element, &mut tree),
            Some(ids[2].clone()),
            "Tab moves on from it"
        );
        run(
            &mut element,
            &mut tree,
            Box::new(focusable::focus_previous()),
        );
        assert_eq!(
            focused(&mut element, &mut tree),
            Some(ids[0].clone()),
            "and does not return to it"
        );
    }
}
