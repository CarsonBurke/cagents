//! Shell effects and input layers that retain native accessibility trees.

use std::{borrow::Cow, cell::RefCell, sync::Arc, time::Duration};

use cosmic::cosmic_theme::{Component, Container, palette::Srgba};
use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{
    Animation, Background, Border, Color, Event, Gradient, Length, Point, Radians, Rectangle,
    Shadow, Size, Vector, gradient::Linear, mouse, time::Instant, window,
};
use cosmic::{Element, Renderer, Theme, widget};
use serde::Deserialize;

mod composer_popover;
pub use composer_popover::composer_popover;
mod context_menu;
mod keyed_list;
mod list_navigation;
pub use list_navigation::reveal_list_selection;

use crate::design::space;

/// Native menu hints come from the same grammar as shortcut dispatch.
pub(crate) fn menu_shortcuts<'a, A: Clone + 'a>(
    bindings: impl IntoIterator<Item = (&'a [String], A)>,
) -> std::collections::HashMap<widget::menu::KeyBind, A> {
    bindings
        .into_iter()
        .filter_map(|(chords, action)| {
            chords
                .iter()
                .find_map(|chord| crate::workspace_widget::menu_keybind(chord))
                .map(|key| (key, action))
        })
        .collect()
}

/// How far a tooltip sits from the control it names, and how wide it may
/// grow before its label wraps.
const TIP_GAP: f32 = space::XS;
const TIP_MAX_WIDTH: f32 = 320.0;

/// Let a focused field answer Escape before its native blur handler runs.
pub fn focused_escape<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_escape: impl Fn() -> Message + 'a,
) -> Element<'a, Message> {
    focused_key(content, move |event| {
        matches!(
            event,
            cosmic::iced::keyboard::Event::KeyPressed {
                key: cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape),
                ..
            }
        )
        .then(|| on_escape())
    })
}

/// Route a key from the field that actually owns native focus, before the
/// native input consumes it. Other fields and already captured events keep
/// their own keyboard handling, including after programmatic focus changes.
pub fn focused_key<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_key: impl Fn(&cosmic::iced::keyboard::Event) -> Option<Message> + 'a,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::FocusedKey,
        tip: None,
        on_focused_key: Some(Box::new(on_key)),
    }
    .into()
}

/// Answer a key that nothing inside `content` claimed, wherever focus is. An
/// open context menu claims the Escape that closes it, for example, so only a
/// second Escape reaches the surface around it. Unlike an application-wide key
/// subscription, nothing outside `content` can claim the key first.
pub fn unclaimed_key<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_key: impl Fn(&cosmic::iced::keyboard::Event) -> Option<Message> + 'a,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::UnclaimedKey,
        tip: None,
        on_focused_key: Some(Box::new(on_key)),
    }
    .into()
}

#[derive(Default)]
struct ChildFocus(bool);

impl Operation for ChildFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        self.0 |= state.is_focused();
    }
}

/// Arrow navigation stays inside the list that owns native keyboard focus.
pub fn keyboard_list<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    enabled: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::KeyboardList(enabled),
        tip: None,
        on_focused_key: None,
    }
    .into()
}

pub fn keyboard_list_row<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::ListRow,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Keyed row state with the standard column's accessibility traversal.
pub fn keyed_list_rows<'a, Message: 'a>(
    rows: Vec<([u8; 32], Element<'a, Message>)>,
) -> Element<'a, Message> {
    keyed_list::view(rows)
}

/// Keyed horizontal controls retain native focus when their order changes.
pub fn keyed_control_row<'a, Message: 'a>(
    controls: Vec<([u8; 32], Element<'a, Message>)>,
    spacing: f32,
    on_focus: impl Fn(Option<[u8; 32]>) + 'a,
) -> Element<'a, Message> {
    keyed_list::horizontal_observed(controls, spacing, Some(Box::new(on_focus)))
}

/// A filter field at the head of a keyboard list. Down enters its first
/// result; editing keys (including Home/End) remain the field's own.
pub fn keyboard_list_entry<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::ListEntry,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

struct ListRowMarker {
    entry: bool,
}

#[derive(Default)]
struct ListFocus {
    rows: Vec<Rectangle>,
    focused: Option<usize>,
    active: Option<usize>,
    active_entry: bool,
    focused_entry: bool,
}

impl Operation for ListFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_ref::<ListRowMarker>() {
            self.active_entry = marker.entry;
            self.active = Some(self.rows.len());
            self.rows.push(bounds);
        }
    }
    fn focusable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if let Some(index) = self.active.take()
            && state.is_focused()
        {
            self.focused = Some(index);
            self.focused_entry = self.active_entry;
        }
    }
}

struct MoveListFocus {
    target: usize,
    index: usize,
    active: Option<usize>,
    bounds: Rectangle,
}

impl Operation for MoveListFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn std::any::Any) {
        if state.is::<ListRowMarker>() {
            self.active = Some(self.index);
            self.index += 1;
        }
    }
    fn focusable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if let Some(index) = self.active.take() {
            if index == self.target {
                state.focus();
            } else {
                state.unfocus();
            }
        }
    }
    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        _translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        let delta = if self.bounds.y < bounds.y {
            self.bounds.y - bounds.y
        } else if self.bounds.y + self.bounds.height > bounds.y + bounds.height {
            self.bounds.y + self.bounds.height - bounds.y - bounds.height
        } else {
            0.0
        };
        if delta != 0.0 {
            state.scroll_by(
                operation::scrollable::AbsoluteOffset { x: 0.0, y: delta },
                bounds,
                content,
            );
        }
    }
}

const HOVER_DURATION: Duration = Duration::from_millis(120);
const TOOLTIP_DELAY: Duration = Duration::from_millis(300);

pub fn font_zoom_chord(
    key: &cosmic::iced::keyboard::Key,
    physical: cosmic::iced::keyboard::key::Physical,
    modifiers: cosmic::iced::keyboard::Modifiers,
) -> Option<i8> {
    use cosmic::iced::keyboard::{
        Key,
        key::{Code, Physical},
    };
    if !modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    match (key.as_ref(), physical) {
        (Key::Character("=" | "+"), _) | (_, Physical::Code(Code::Equal)) => Some(1),
        (Key::Character("-"), _) | (_, Physical::Code(Code::Minus)) if !modifiers.shift() => {
            Some(-1)
        }
        (Key::Character("0"), _) | (_, Physical::Code(Code::Digit0)) if !modifiers.shift() => {
            Some(0)
        }
        _ => None,
    }
}

/// Reserve tab-history keys before editor widgets can treat Tab as indentation.
/// The application's keyboard subscription owns navigation and Control release.
pub fn reserve_tab_history<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    enabled: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::TabHistory(enabled),
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Soft full-bounds hover feedback, including when a child captures the event.
/// The native animation clock requests frames only until its target is reached.
pub fn hover<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    hover_with_radius(content, false)
}

/// Hover feedback matching the theme radius used by sidebar row controls.
pub fn hover_with_theme_radius<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    hover_with_radius(content, true)
}

fn hover_with_radius<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    themed_radius: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Hover { themed_radius },
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Keeps passive window-frame subscriptions advancing only while state animates.
pub fn drive_animation<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    active: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::DriveAnimation(active),
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Reveal a right-anchored pane without reflowing its contents on every frame.
pub fn reveal_width<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
) -> Element<'a, Message> {
    reveal_width_aligned(content, width, progress, false)
}

/// Treat a vertical mouse wheel as horizontal motion.
///
/// iced's native scrollable only swaps axes while Shift is held. Tab strips
/// are horizontal-only, so a normal wheel should move them without a modifier.
pub fn horizontal_wheel<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(WheelInput {
        content: content.into(),
        route: WheelRoute::Horizontal,
    })
}

/// Treat a vertical mouse wheel as horizontal motion, except over a nested
/// scrollable that moves vertically, which keeps it.
///
/// A row of vertically scrolling lanes inside a horizontal scrollable wants
/// both: the wheel scrolls the lane under the pointer, and over the headings
/// and gaps between lanes it moves the row sideways. A lane already at its
/// end keeps the wheel too, so the row never slides out from under a pointer
/// that is scrolling a lane; a sideways trackpad swipe still moves the row.
pub fn horizontal_wheel_around_vertical<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(WheelInput {
        content: content.into(),
        route: WheelRoute::AroundVertical,
    })
}

/// Lets the enclosing native scrollable own wheel gestures over a read-only
/// editor while preserving the editor's selection and keyboard handling.
pub fn parent_wheel<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(WheelInput {
        content: content.into(),
        route: WheelRoute::Parent,
    })
}

/// Fade the leading/trailing edges of a horizontal strip when it can scroll.
///
/// Tab rows hide their scrollbar; the fade is the usual hint that more tabs
/// sit off-screen.
pub fn overflow_fade_x<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(OverflowFadeX {
        content: content.into(),
    })
}

/// Projects a wheel gesture onto the horizontal axis, taking the axis it
/// mostly moves along.
///
/// Summing the two components cancels a diagonal trackpad gesture exactly - a
/// gesture reporting `x = -d, y = +d` sums to zero - and [`WheelInput`]
/// has already replaced the original event by then, so the motion is discarded
/// rather than forwarded. Only one axis is taken, by magnitude rather than by
/// `x == 0`: a trackpad reports a small nonzero `x` on a two-finger drag that
/// is plainly vertical, and testing for exact zero would hand the gesture to
/// that jitter. A tie goes to the horizontal axis.
fn wheel_as_horizontal(delta: mouse::ScrollDelta) -> Event {
    fn horizontal(x: f32, y: f32) -> f32 {
        if mostly_vertical(x, y) { y } else { x }
    }
    let x = match delta {
        mouse::ScrollDelta::Lines { x, y } => horizontal(x, y) * 60.0,
        mouse::ScrollDelta::Pixels { x, y } => horizontal(x, y),
    };
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x, y: 0.0 },
    })
}

/// Reveal a left-anchored pane without reflowing its contents on every frame.
pub fn reveal_leading_width<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
) -> Element<'a, Message> {
    reveal_width_aligned(content, width, progress, true)
}

fn reveal_width_aligned<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    progress: f32,
    leading: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::RevealWidth {
            width: width.max(0.0),
            progress: progress.clamp(0.0, 1.0),
            leading,
        },
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Fades semantic COSMIC surfaces and text without moving them.
///
/// This is not a renderer opacity group: the pinned renderer has no group-alpha
/// operation, and its SVG recoloring ignores color alpha, even for symbolic
/// icons. Icons/images must use their own `.opacity(progress)`; explicit custom
/// colors and styles which replace alpha must also apply `progress` themselves.
/// Child events, focus, overlays, and accessibility remain available throughout.
pub fn fade<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    progress: f32,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Fade(progress.clamp(0.0, 1.0)),
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Delayed, stationary tooltips that do not hide their control from assistive
/// technology (the pinned upstream Tooltip does not forward a11y nodes).
/// Give the control its own accessible name/description as well. The dwell clock
/// follows the control, not its label, so live status text cannot postpone it.
pub fn tooltip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'a, str>>,
) -> Element<'a, Message> {
    tooltip_positioned(content, label, false)
}

/// Keep a tooltip above chrome whose native child surface covers the area below.
pub fn tooltip_above<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'a, str>>,
) -> Element<'a, Message> {
    tooltip_positioned(content, label, true)
}

fn tooltip_positioned<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'a, str>>,
    prefer_above: bool,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Tooltip,
        tip: Some(Tip {
            label: label.into(),
            prefer_above,
            content: None,
        }),
        on_focused_key: None,
    }
    .into()
}

/// Keep the native menu beside its content: the pinned COSMIC ContextMenu
/// does not forward `mouse_interaction`, so wrapping the content inside it
/// loses every child cursor. The transparent front layer owns menu gestures;
/// the real content retains its native input, cursor and accessibility tree.
/// Schedule a frame when a stationary right click opens the menu.
pub fn context_menu<'a, Message: Clone + 'static>(
    content: impl Into<Element<'a, Message>>,
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    Effect {
        content: layer(content.into(), context_menu::surface(menu)),
        kind: Kind::ContextMenu,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// [`context_menu`] whose entries are built only when the menu opens, for
/// rows whose menu would otherwise be assembled on every view.
pub fn context_menu_lazy<'a, Message: Clone + 'static>(
    content: impl Into<Element<'a, Message>>,
    builder: impl Fn() -> Vec<widget::menu::Tree<Message>> + 'a,
) -> Element<'a, Message> {
    Effect {
        content: layer(content.into(), context_menu::lazy_surface(builder)),
        kind: Kind::ContextMenu,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Keeps every key from the layers beneath a transient surface that the
/// application's keyboard subscription drives, so a key that steers or
/// dismisses it is not also typed into the pane it covers.
pub fn keyboard_modal<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::KeyboardModal,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Keeps Tab, Shift+Tab and the Left and Right arrows moving keyboard focus
/// among the controls inside `content`, wrapping at either end, so a dialog's
/// choices are one key apart and focus never wanders to the window behind it.
/// A key a focused control claimed, such as an arrow in a text field, is left
/// to that control.
pub fn focus_scope<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::FocusScope,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Counts the focusable controls in a subtree and finds the focused one.
/// Selectable text registers as focusable without an id; it is for the
/// pointer to copy from, not a stop on the way between a dialog's choices.
#[derive(Default)]
struct ScopeFocus {
    count: usize,
    focused: Option<usize>,
}

impl Operation for ScopeFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if id.is_none() {
            return;
        }
        if state.is_focused() {
            self.focused = Some(self.count);
        }
        self.count += 1;
    }
}

/// Focuses the subtree's `target`th focusable control and no other.
struct FocusNth {
    target: usize,
    index: usize,
}

impl Operation for FocusNth {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if id.is_some() && self.index == self.target {
            state.focus();
        } else {
            state.unfocus();
        }
        self.index += usize::from(id.is_some());
    }
}

/// Where Tab, Shift+Tab, Left or Right moves focus within a scope.
fn scope_step(event: &Event) -> Option<bool> {
    use cosmic::iced::keyboard::{self, key::Named};
    let Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key),
        modifiers,
        ..
    }) = event
    else {
        return None;
    };
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    match key {
        Named::Tab => Some(modifiers.shift()),
        Named::ArrowLeft if !modifiers.shift() => Some(true),
        Named::ArrowRight if !modifiers.shift() => Some(false),
        _ => None,
    }
}

/// A key held down long enough to repeat Enter or Space.
fn is_held_activation(event: &Event) -> bool {
    use cosmic::iced::keyboard::{self, key::Named};
    matches!(
        event,
        Event::Keyboard(keyboard::Event::KeyPressed { key, repeat: true, .. })
            if *key == keyboard::Key::Named(Named::Enter)
                || *key == keyboard::Key::Character(" ".into())
    )
}

/// A printable key other than Space, pressed without a command modifier.
fn is_typed_text(event: &Event) -> bool {
    use cosmic::iced::keyboard;
    matches!(
        event,
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(character),
            modifiers,
            ..
        }) if character.as_str() != " "
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
    )
}

/// An unmodified Space press restated as Enter, the key that presses a
/// focused button.
fn space_as_enter(event: &Event) -> Option<Event> {
    use cosmic::iced::keyboard::{self, key::Named};
    let Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Character(character),
        physical_key,
        location,
        modifiers,
        repeat,
        ..
    }) = event
    else {
        return None;
    };
    if character.as_str() != " "
        || modifiers.control()
        || modifiers.alt()
        || modifiers.logo()
        || modifiers.shift()
    {
        return None;
    }
    let enter = keyboard::Key::Named(Named::Enter);
    Some(Event::Keyboard(keyboard::Event::KeyPressed {
        key: enter.clone(),
        modified_key: enter,
        physical_key: *physical_key,
        location: *location,
        modifiers: *modifiers,
        text: None,
        repeat: *repeat,
    }))
}

/// Blocks click-through without hiding the child's accessibility tree.
pub fn opaque<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Effect {
        content: content.into(),
        kind: Kind::Opaque,
        tip: None,
        on_focused_key: None,
    }
    .into()
}

/// Two visual layers with front-first input and complete accessibility traversal.
/// The pinned upstream Stack drops accessibility nodes for all descendants.
pub fn layer<'a, Message: 'a>(
    base: Element<'a, Message>,
    front: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Layers {
        children: [base, front],
        front_exclusive: false,
    })
}

/// Retain the base tree while a pane-local chooser owns input, accessibility
/// and drawing. A stable second slot prevents opening/closing it from replacing
/// the transcript, and nothing of the base, such as a scrollbar beside a
/// narrower chooser, shows or answers the pointer while it is covered.
pub fn exclusive_layer<'a, Message: 'a>(
    base: Element<'a, Message>,
    front: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let front_exclusive = front.is_some();
    Element::new(Layers {
        children: [
            base,
            front.unwrap_or_else(|| cosmic::iced::widget::Space::new().into()),
        ],
        front_exclusive,
    })
}

struct Layers<'a, Message> {
    children: [Element<'a, Message>; 2],
    front_exclusive: bool,
}

impl<Message> Widget<Message, Theme, Renderer> for Layers<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        self.children[0].as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let base = self.children[0]
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let size = base.size();
        let front = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        layout::Node::with_children(size, vec![base, front])
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
            for (index, ((child, tree), layout)) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(
                    layout
                        .children()
                        .map(|child| child.with_virtual_offset(layout.virtual_offset())),
                )
                .enumerate()
            {
                if self.front_exclusive && index == 0 {
                    continue;
                }
                child
                    .as_widget_mut()
                    .operate(tree, layout, renderer, operation);
            }
        });
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        mut cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let over = cursor.is_over(layout.bounds());
        let exclusive_viewport = layout.bounds().intersection(viewport).unwrap_or_default();
        if self.front_exclusive && !over {
            cursor = mouse::Cursor::Unavailable;
        }
        let viewport = if self.front_exclusive {
            &exclusive_viewport
        } else {
            viewport
        };
        for ((child, tree), layout) in self
            .children
            .iter_mut()
            .rev()
            .zip(tree.children.iter_mut().rev())
            .zip(
                layout
                    .children()
                    .rev()
                    .map(|child| child.with_virtual_offset(layout.virtual_offset())),
            )
        {
            child.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
            if self.front_exclusive || shell.is_event_captured() {
                return;
            }
            if over
                && !cursor.is_levitating()
                && child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
                    != mouse::Interaction::None
            {
                cursor = cursor.levitate();
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
        self.children
            .iter()
            .rev()
            .zip(tree.children.iter().rev())
            .zip(
                layout
                    .children()
                    .rev()
                    .map(|child| child.with_virtual_offset(layout.virtual_offset())),
            )
            .take(if self.front_exclusive { 1 } else { 2 })
            .map(|((child, tree), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
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
        if layout.bounds().intersection(viewport).is_none() {
            return;
        }
        let mut layouts = layout
            .children()
            .map(|child| child.with_virtual_offset(layout.virtual_offset()));
        let base = layouts.next().expect("base layer");
        let front = layouts.next().expect("front layer");
        if !self.front_exclusive {
            let front_active = self.children[1].as_widget().mouse_interaction(
                &tree.children[1],
                front,
                cursor,
                viewport,
                renderer,
            ) != mouse::Interaction::None;
            self.children[0].as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                base,
                if front_active {
                    mouse::Cursor::Unavailable
                } else {
                    cursor
                },
                viewport,
            );
        }
        let front_viewport = if self.front_exclusive {
            layout.bounds().intersection(viewport).unwrap_or_default()
        } else {
            *viewport
        };
        renderer.with_layer(front_viewport, |renderer| {
            self.children[1].as_widget().draw(
                &tree.children[1],
                renderer,
                theme,
                style,
                front,
                cursor,
                &front_viewport,
            )
        });
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        if self.front_exclusive {
            return self.children[1].as_widget_mut().overlay(
                &mut tree.children[1],
                layout.children().nth(1).expect("front layer"),
                renderer,
                viewport,
                translation,
            );
        }
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
                .zip(
                    layout
                        .children()
                        .map(|child| child.with_virtual_offset(layout.virtual_offset())),
                )
                .enumerate()
                .filter(|(index, _)| !self.front_exclusive || *index == 1)
                .map(|(_, ((child, tree), layout))| {
                    child.as_widget().a11y_nodes(layout, tree, cursor)
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
        for (index, ((child, tree), child_layout)) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .enumerate()
        {
            if self.front_exclusive && index == 0 {
                continue;
            }
            child.as_widget().drag_destinations(
                tree,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

struct WheelInput<'a, Message> {
    content: Element<'a, Message>,
    route: WheelRoute,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WheelRoute {
    /// Every wheel moves the content sideways.
    Horizontal,
    /// A wheel moving mostly vertically stays as it is over a scrollable
    /// that moves vertically, even one already at its end; any other wheel
    /// moves the content sideways.
    AroundVertical,
    /// The wheel skips the content for the enclosing scrollable.
    Parent,
}

/// The wheel gesture in progress, and whether it keeps its vertical motion.
///
/// iced's scrollable keeps a gesture on the scrollable it started on until
/// the pointer moves well after the last wheel, a button changes, or the
/// wheel rests; ending ours by the same rules keeps the two agreeing. A
/// gesture that moved the content sideways therefore goes on doing so when
/// that motion brings a lane under the pointer.
#[derive(Default)]
struct WheelGesture {
    current: Option<(bool, Instant)>,
    /// Shift asks iced's scrollables for sideways motion, so a wheel with it
    /// held never keeps its vertical motion.
    shift: bool,
}

impl WheelGesture {
    /// iced's scrollable: a pointer motion this soon after a wheel belongs
    /// to the gesture.
    const MOTION_GRACE: Duration = Duration::from_millis(100);
    /// iced's scrollable: a wheel at rest this long has ended its gesture.
    const REST: Duration = Duration::from_millis(1500);

    fn observe(&mut self, event: &Event) {
        let ended = match event {
            Event::Keyboard(cosmic::iced::keyboard::Event::ModifiersChanged(modifiers)) => {
                let changed = modifiers.shift() != self.shift;
                self.shift = modifiers.shift();
                changed
            }
            Event::Mouse(
                mouse::Event::ButtonPressed(_)
                | mouse::Event::ButtonReleased(_)
                | mouse::Event::CursorLeft,
            ) => true,
            Event::Mouse(mouse::Event::CursorMoved { .. }) => self
                .current
                .is_some_and(|(_, at)| at.elapsed() > Self::MOTION_GRACE),
            _ => false,
        };
        if ended {
            self.current = None;
        }
    }

    /// Whether this wheel keeps its vertical motion: as the gesture decided
    /// if one is in progress, otherwise as `decide` says. Only the first
    /// wheel of a gesture pays for deciding.
    fn keeps_vertical(&mut self, decide: impl FnOnce() -> bool) -> bool {
        let keep = match self.current {
            Some((keep, at)) if at.elapsed() <= Self::REST => keep,
            _ => !self.shift && decide(),
        };
        self.current = Some((keep, Instant::now()));
        keep
    }
}

/// Finds whether a scrollable taller than its bounds - one that moves
/// vertically - lies under a point given in the operated layout's
/// coordinates.
///
/// Only the content of a scrollable under the point is visited, so a
/// scrollable scrolled out of its parent's sight is never found, and the
/// search costs about one visit per scrollable on the way to the point
/// rather than one per widget: on a board, the lanes, not their cards.
struct VerticalScrollableAt {
    point: Point,
    /// Scrolled distance of the content being visited.
    offset: Vector,
    /// What the next traversal, a just visited scrollable's content, does.
    next: Enter,
    found: bool,
}

enum Enter {
    /// Not a scrollable's content: visited as it is.
    Plain,
    /// The content of a scrollable under the point, scrolled this far.
    Scrolled(Vector),
    /// The content of a scrollable away from the point.
    Skip,
}

impl VerticalScrollableAt {
    fn new(point: Point) -> Self {
        Self {
            point,
            offset: Vector::ZERO,
            next: Enter::Plain,
            found: false,
        }
    }
}

impl Operation for VerticalScrollableAt {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let outer = self.offset;
        match std::mem::replace(&mut self.next, Enter::Plain) {
            _ if self.found => return,
            Enter::Skip => return,
            Enter::Scrolled(translation) => self.offset = outer + translation,
            Enter::Plain => {}
        }
        operate(self);
        self.offset = outer;
    }

    fn container(&mut self, _id: Option<&Id>, _bounds: Rectangle) {
        self.next = Enter::Plain;
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        if self.found {
            return;
        }
        if bounds.contains(self.point + self.offset) {
            self.found = content_bounds.height > bounds.height;
            self.next = Enter::Scrolled(translation);
        } else {
            self.next = Enter::Skip;
        }
    }
}

/// Whether a wheel moves more vertically than horizontally.
fn mostly_vertical(x: f32, y: f32) -> bool {
    y.abs() > x.abs()
}

impl<Message> Widget<Message, Theme, Renderer> for WheelInput<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WheelGesture>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(WheelGesture::default())
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
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.route == WheelRoute::Parent
            && matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
        {
            return;
        }
        let gesture = tree.state.downcast_mut::<WheelGesture>();
        gesture.observe(event);
        let remapped;
        let event = match (event, cursor.position_over(layout.bounds())) {
            (Event::Mouse(mouse::Event::WheelScrolled { delta }), Some(point)) => {
                let keep = self.route == WheelRoute::AroundVertical
                    && gesture.keeps_vertical(|| {
                        let mut under = VerticalScrollableAt::new(point);
                        let (mouse::ScrollDelta::Lines { x, y }
                        | mouse::ScrollDelta::Pixels { x, y }) = *delta;
                        mostly_vertical(x, y) && {
                            self.content.as_widget_mut().operate(
                                &mut tree.children[0],
                                layout.with_virtual_offset(layout.virtual_offset()),
                                renderer,
                                &mut under,
                            );
                            under.found
                        }
                    });
                if keep {
                    event
                } else {
                    remapped = wheel_as_horizontal(*delta);
                    &remapped
                }
            }
            (other, _) => other,
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().a11y_nodes(
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

const OVERFLOW_FADE_WIDTH: f32 = 24.0;

fn overflow_edges(offset: f32, viewport: f32, content: f32) -> (bool, bool) {
    (offset > 0.5, content - offset > viewport + 0.5)
}

struct OverflowFadeX<'a, Message> {
    content: Element<'a, Message>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct OverflowState {
    start: bool,
    end: bool,
}

struct OverflowProbe {
    start: bool,
    end: bool,
    found: bool,
}

impl<T> Operation<T> for OverflowProbe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<T>)) {
        // The strip is found near the root; every tab beneath it would
        // otherwise be walked on each event and layout for nothing.
        if !self.found {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        // The tab strip is the outermost scrollable. Nested ones (editor
        // panes, menus) would otherwise overwrite the fade with their offset.
        if self.found {
            return;
        }
        self.found = true;
        (self.start, self.end) = overflow_edges(translation.x, bounds.width, content_bounds.width);
    }
}

fn read_overflow<Message>(
    content: &mut Element<'_, Message>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &Renderer,
) -> OverflowState {
    let mut probe = OverflowProbe {
        start: false,
        end: false,
        found: false,
    };
    content
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut probe);
    OverflowState {
        start: probe.start,
        end: probe.end,
    }
}

impl<Message> Widget<Message, Theme, Renderer> for OverflowFadeX<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<OverflowState>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(OverflowState::default())
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
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let overflow = read_overflow(
            &mut self.content,
            &mut tree.children[0],
            Layout::new(&node),
            renderer,
        );
        *tree.state.downcast_mut::<OverflowState>() = overflow;
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let child_layout = layout.with_virtual_offset(layout.virtual_offset());
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let overflow = read_overflow(
            &mut self.content,
            &mut tree.children[0],
            child_layout,
            renderer,
        );
        let state = tree.state.downcast_mut::<OverflowState>();
        if *state != overflow {
            *state = overflow;
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<OverflowState>();
        if !state.start && !state.end {
            return;
        }
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        // Match the chrome behind the tabs, not the component fill sitting on it.
        let mut color: Color = theme.current_container().base.into();
        color.a = 1.0;
        let mut clear = color;
        clear.a = 0.0;
        renderer.with_layer(clip, |renderer| {
            if state.start {
                fill_overflow_fade(
                    renderer,
                    Rectangle {
                        x: bounds.x,
                        y: bounds.y,
                        width: OVERFLOW_FADE_WIDTH.min(bounds.width),
                        height: bounds.height,
                    },
                    color,
                    clear,
                );
            }
            if state.end {
                let width = OVERFLOW_FADE_WIDTH.min(bounds.width);
                fill_overflow_fade(
                    renderer,
                    Rectangle {
                        x: bounds.x + bounds.width - width,
                        y: bounds.y,
                        width,
                        height: bounds.height,
                    },
                    clear,
                    color,
                );
            }
        });
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().a11y_nodes(
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

fn fill_overflow_fade(renderer: &mut Renderer, bounds: Rectangle, from: Color, to: Color) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        // iced treats 0 rad as upward (CSS 0deg). π/2 is left-to-right.
        Background::Gradient(Gradient::Linear(
            Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                .add_stop(0.0, from)
                .add_stop(1.0, to),
        )),
    );
}

#[derive(Clone, Copy)]
enum Kind {
    KeyboardList(bool),
    FocusedKey,
    ListRow,
    ListEntry,
    TabHistory(bool),
    Hover {
        themed_radius: bool,
    },
    ContextMenu,
    Fade(f32),
    Tooltip,
    DriveAnimation(bool),
    RevealWidth {
        width: f32,
        progress: f32,
        leading: bool,
    },
    Opaque,
    KeyboardModal,
    UnclaimedKey,
    FocusScope,
}

struct Effect<'a, Message> {
    content: Element<'a, Message>,
    kind: Kind,
    tip: Option<Tip<'a, Message>>,
    on_focused_key: Option<Box<dyn Fn(&cosmic::iced::keyboard::Event) -> Option<Message> + 'a>>,
}

struct Tip<'a, Message> {
    label: Cow<'a, str>,
    prefer_above: bool,
    /// The tip's surface, built when it first opens. A view names a tooltip
    /// for nearly every control and shows at most one of them, so the text,
    /// its container and their widget state are not built until then.
    content: Option<Element<'a, Message>>,
}

impl<'a, Message: 'a> Tip<'a, Message> {
    fn content(&mut self) -> &mut Element<'a, Message> {
        self.content
            .get_or_insert_with(|| tip_surface(self.label.clone()))
    }
}

/// What a tooltip's surface holds: its label, padded and wrapped.
fn tip_surface<'a, Message: 'a>(label: Cow<'a, str>) -> Element<'a, Message> {
    // The shell's type scale owns this size. Tooltip text is the whole
    // content of its surface rather than secondary metadata beside something
    // else, so it takes the body role; `design::caption` also mutes its
    // color, which an overlay with its own background must not do.
    cosmic::widget::container(crate::design::body(label))
        .class(crate::design::transparent())
        .padding([space::XS, space::S])
        .max_width(TIP_MAX_WIDTH)
        .into()
}

/// A tooltip for part of a widget rather than a whole control, such as one
/// link among a paragraph's words, with the surface, delay and fade of
/// [`tooltip`]. Its owner keeps it while the pointer stays on that part and
/// drops it when the pointer leaves, and asks for frames until it
/// [settles](Self::settles_at).
pub(crate) struct PartTip<Message> {
    label: Arc<str>,
    content: Element<'static, Message>,
    tree: Tree,
    hovered: Instant,
}

impl<Message: 'static> PartTip<Message> {
    pub(crate) fn new(label: Arc<str>, hovered: Instant) -> Self {
        let content = tip_surface(Cow::Owned(label.to_string()));
        let tree = Tree::new(&content);
        Self {
            label,
            content,
            tree,
            hovered,
        }
    }
    pub(crate) fn label(&self) -> &Arc<str> {
        &self.label
    }
    /// When the tip starts to fade in.
    pub(crate) fn opens_at(&self) -> Instant {
        self.hovered + TOOLTIP_DELAY
    }
    /// When the tip has finished fading in.
    pub(crate) fn settles_at(&self) -> Instant {
        self.hovered + TOOLTIP_DELAY + HOVER_DURATION
    }
    /// The tip beside `anchor`, the part's bounds in the overlay's frame,
    /// once the pointer has rested there long enough.
    pub(crate) fn overlay(
        &mut self,
        anchor: Rectangle,
        now: Instant,
    ) -> Option<overlay::Element<'_, Message, Theme, Renderer>> {
        let shown = now.checked_duration_since(self.hovered + TOOLTIP_DELAY)?;
        let progress = (shown.as_secs_f32() / HOVER_DURATION.as_secs_f32()).min(1.0);
        Some(overlay::Element::new(Box::new(TipOverlay {
            content: &mut self.content,
            tree: &mut self.tree,
            anchor,
            progress,
            prefer_above: false,
        })))
    }
}

// Distinct tags prevent a positional diff from reusing a tooltip's clock as a
// hover/fade clock when a conditional wrapper changes kind.
struct HoverTag;
struct TabHistoryTag;
struct KeyboardListTag;
struct FocusedKeyTag;
struct UnclaimedKeyTag;
struct ListRowTag;
struct FadeTag;
struct TooltipTag;
struct AnimationDriverTag;
struct OpaqueTag;
struct KeyboardModalTag;
struct FocusScopeTag;
struct ContextMenuTag;
struct RevealWidthTag;

fn effect_layout(kind: Kind, layout: Layout<'_>) -> Layout<'_> {
    if matches!(kind, Kind::RevealWidth { .. }) {
        layout
            .children()
            .next()
            .expect("revealed pane layout")
            .with_virtual_offset(layout.virtual_offset())
    } else {
        layout
    }
}

/// Where the pointer last was on a control whose press hid its tooltip, and
/// whether the pointer has since been
/// hidden from the control. A menu the press opened, or an ancestor gating
/// the cursor to its own region, reports no position without saying where
/// the pointer went; seeing it again anywhere but the press point means it
/// moved away and came back.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PressSuppression {
    at: Option<Point>,
    obscured: bool,
}

impl PressSuppression {
    /// Whether this cursor shows the pointer has left the control.
    fn released_by(&mut self, cursor: mouse::Cursor, bounds: Option<Rectangle>) -> bool {
        match cursor.position() {
            None => {
                self.obscured = true;
                false
            }
            Some(position) if !bounds.is_some_and(|bounds| bounds.contains(position)) => true,
            Some(position) if self.obscured => self.at != Some(position),
            // Follow the pointer until it is hidden: a context menu opens on
            // the release, which may land a pixel away from the press.
            Some(position) => {
                self.at = Some(position);
                false
            }
        }
    }
}

struct State {
    hover: Animation<bool>,
    tooltip: Animation<bool>,
    entered: Option<Instant>,
    tip_open: bool,
    /// Set by a press on the control; its tooltip stays hidden until the
    /// pointer really leaves.
    tip_suppressed: Option<PressSuppression>,
    inspect_control: bool,
    disabled: bool,
    focused: bool,
    now: Instant,
    faded: RefCell<Option<FadedTheme>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            hover: Animation::new(false).duration(HOVER_DURATION),
            tooltip: Animation::new(false).duration(HOVER_DURATION),
            entered: None,
            tip_open: false,
            tip_suppressed: None,
            inspect_control: true,
            disabled: false,
            focused: true,
            now: Instant::now(),
            faded: RefCell::new(None),
        }
    }
}

impl<'a, Message: 'a> Widget<Message, Theme, Renderer> for Effect<'a, Message> {
    fn tag(&self) -> tree::Tag {
        match self.kind {
            Kind::KeyboardList(_) => tree::Tag::of::<KeyboardListTag>(),
            Kind::FocusedKey => tree::Tag::of::<FocusedKeyTag>(),
            Kind::UnclaimedKey => tree::Tag::of::<UnclaimedKeyTag>(),
            Kind::ListRow | Kind::ListEntry => tree::Tag::of::<ListRowTag>(),
            Kind::TabHistory(_) => tree::Tag::of::<TabHistoryTag>(),
            Kind::Hover { .. } => tree::Tag::of::<HoverTag>(),
            Kind::Fade(_) => tree::Tag::of::<FadeTag>(),
            Kind::Tooltip => tree::Tag::of::<TooltipTag>(),
            Kind::DriveAnimation(_) => tree::Tag::of::<AnimationDriverTag>(),
            Kind::Opaque => tree::Tag::of::<OpaqueTag>(),
            Kind::KeyboardModal => tree::Tag::of::<KeyboardModalTag>(),
            Kind::FocusScope => tree::Tag::of::<FocusScopeTag>(),
            Kind::ContextMenu => tree::Tag::of::<ContextMenuTag>(),
            Kind::RevealWidth { .. } => tree::Tag::of::<RevealWidthTag>(),
        }
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let identity = control_identity(&tree.children[0]).cloned();
        // An open tip's state sits after the content's and is rebuilt by
        // `overlay`; a new view starts from the content alone.
        tree.diff_children(std::slice::from_mut(&mut self.content));
        let changed_control = identity.as_ref() != control_identity(&tree.children[0]);
        let state = tree.state.downcast_mut::<State>();
        state.inspect_control = true;
        if changed_control {
            *state = State::default();
        }
    }
    fn size(&self) -> Size<Length> {
        let mut size = self.content.as_widget().size();
        if let Kind::RevealWidth {
            width, progress, ..
        } = self.kind
        {
            size.width = Length::Fixed(width * progress);
        }
        size
    }
    fn size_hint(&self) -> Size<Length> {
        if matches!(self.kind, Kind::RevealWidth { .. }) {
            self.size()
        } else {
            self.content.as_widget().size_hint()
        }
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        if let Kind::RevealWidth {
            width,
            progress,
            leading,
        } = self.kind
        {
            let visible = (width * progress).min(limits.max().width);
            let child = self.content.as_widget_mut().layout(
                &mut tree.children[0],
                renderer,
                &layout::Limits::new(Size::new(width, 0.0), Size::new(width, limits.max().height)),
            );
            let height = child.size().height;
            let x = if leading { 0.0 } else { visible - width };
            return layout::Node::with_children(
                Size::new(visible, height),
                vec![child.move_to(Point::new(x, 0.0))],
            );
        }
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
        if matches!(self.kind, Kind::ListRow | Kind::ListEntry) {
            operation.custom(
                None,
                layout.bounds() - layout.virtual_offset(),
                &mut ListRowMarker {
                    entry: matches!(self.kind, Kind::ListEntry),
                },
            );
        }
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            effect_layout(self.kind, layout),
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
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if matches!(self.kind, Kind::FocusedKey)
            && let Some(on_key) = &self.on_focused_key
            && let Event::Keyboard(event) = event
        {
            if shell.is_event_captured() {
                return;
            }
            let mut focus = ChildFocus::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            if focus.0
                && let Some(message) = on_key(event)
            {
                shell.publish(message);
                shell.capture_event();
                return;
            }
        }
        if let Kind::KeyboardList(true) = self.kind
            && let Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
                key: cosmic::iced::keyboard::Key::Named(key),
                modifiers,
                ..
            }) = event
            && modifiers.is_empty()
            && matches!(
                key,
                cosmic::iced::keyboard::key::Named::ArrowUp
                    | cosmic::iced::keyboard::key::Named::ArrowDown
                    | cosmic::iced::keyboard::key::Named::Home
                    | cosmic::iced::keyboard::key::Named::End
            )
        {
            let mut focus = ListFocus::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            if let Some(current) = focus.focused
                && (!focus.focused_entry || *key == cosmic::iced::keyboard::key::Named::ArrowDown)
            {
                use cosmic::iced::keyboard::key::Named;
                let target = match key {
                    Named::ArrowUp => current.saturating_sub(1),
                    Named::ArrowDown => (current + 1).min(focus.rows.len().saturating_sub(1)),
                    Named::Home => 0,
                    _ => focus.rows.len().saturating_sub(1),
                };
                let mut movement = MoveListFocus {
                    target,
                    index: 0,
                    active: None,
                    bounds: focus.rows[target],
                };
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut movement,
                );
                shell.capture_event();
                shell.request_redraw();
                return;
            }
        }
        // Modifier state is not a key press: widgets beneath keep their own
        // copy of it for the clicks and drags that follow, and would be left
        // believing a modifier is still held.
        if matches!(self.kind, Kind::KeyboardModal)
            && matches!(
                event,
                Event::Keyboard(
                    cosmic::iced::keyboard::Event::KeyPressed { .. }
                        | cosmic::iced::keyboard::Event::KeyReleased { .. }
                )
            )
        {
            shell.capture_event();
            return;
        }
        if let Kind::TabHistory(true) = self.kind
            && let Event::Keyboard(
                cosmic::iced::keyboard::Event::KeyPressed { key, modifiers, .. }
                | cosmic::iced::keyboard::Event::KeyReleased { key, modifiers, .. },
            ) = event
            && crate::tab_history::is_chord(key, *modifiers)
        {
            shell.capture_event();
            return;
        }
        if let Kind::RevealWidth { progress, .. } = self.kind {
            let clipped = layout.bounds().intersection(viewport).unwrap_or_default();
            let cursor = if progress < 1.0 || !cursor.is_over(clipped) {
                mouse::Cursor::Unavailable
            } else {
                cursor
            };
            if progress < 1.0 && !matches!(event, Event::Window(_)) {
                return;
            }
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                effect_layout(self.kind, layout),
                cursor,
                renderer,
                clipboard,
                shell,
                &clipped,
            );
            return;
        }
        if matches!(self.kind, Kind::FocusScope) && is_held_activation(event) {
            // A held Enter or Space would answer every question it meets,
            // including the next one a queue of closes opens.
            shell.capture_event();
            return;
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
        if matches!(self.kind, Kind::FocusScope) {
            if !shell.is_event_captured()
                && let Some(backwards) = scope_step(event)
            {
                let mut focus = ScopeFocus::default();
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focus,
                );
                if focus.count > 0 {
                    let target = match (focus.focused, backwards) {
                        (Some(current), false) => (current + 1) % focus.count,
                        (Some(current), true) => (current + focus.count - 1) % focus.count,
                        (None, false) => 0,
                        (None, true) => focus.count - 1,
                    };
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut FocusNth { target, index: 0 },
                    );
                    shell.request_redraw();
                }
                // Even with nothing to focus, the key stays in the dialog.
                shell.capture_event();
            } else if !shell.is_event_captured() && is_typed_text(event) {
                // Text no control took was meant for whatever had the
                // keyboard before the question appeared. Whoever is still
                // typing hasn't read it, so the Enter that ends their line
                // must not answer it: focus leaves every choice until Tab
                // or a click picks one.
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut FocusNth {
                        target: usize::MAX,
                        index: 0,
                    },
                );
                shell.request_redraw();
                shell.capture_event();
            } else if !shell.is_event_captured()
                && let Some(enter) = space_as_enter(event)
            {
                // Buttons here answer only Enter. Space, which every
                // toolkit also uses to press a focused button or tick a
                // checkbox, reaches them as Enter once no focused control,
                // such as a text field, has taken it as text.
                self.content.as_widget_mut().update(
                    &mut tree.children[0],
                    &enter,
                    layout,
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                );
                shell.capture_event();
            }
            return;
        }
        if matches!(self.kind, Kind::UnclaimedKey)
            && !shell.is_event_captured()
            && let Event::Keyboard(event) = event
            && let Some(message) = self
                .on_focused_key
                .as_ref()
                .and_then(|on_key| on_key(event))
        {
            shell.publish(message);
            shell.capture_event();
            return;
        }
        if matches!(self.kind, Kind::TabHistory(_)) {
            return;
        }
        if matches!(self.kind, Kind::ContextMenu) {
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right))
            ) && cursor.is_over(layout.bounds())
            {
                shell.invalidate_layout();
                shell.request_redraw();
            }
            return;
        }
        if matches!(self.kind, Kind::Opaque) {
            if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
                && cursor.is_over(layout.bounds())
            {
                shell.capture_event();
            }
            return;
        }
        if let Kind::DriveAnimation(active) = self.kind {
            if active {
                shell.request_redraw();
            }
            return;
        }
        if matches!(self.kind, Kind::Fade(_)) {
            return;
        }
        let state = tree.state.downcast_mut::<State>();
        let now = match event {
            Event::Window(window::Event::RedrawRequested(at)) => *at,
            _ => Instant::now(),
        };
        state.now = now;
        match event {
            Event::Window(window::Event::Focused) => state.focused = true,
            Event::Window(window::Event::Unfocused) => state.focused = false,
            _ => {}
        }
        let mut inside = state.focused
            && !matches!(event, Event::Mouse(mouse::Event::CursorLeft))
            && layout
                .bounds()
                .intersection(viewport)
                .is_some_and(|bounds| cursor.is_over(bounds));
        if !inside {
            state.inspect_control = true;
        }
        // A press keeps the tooltip hidden until the pointer leaves the
        // control or the window, or the window loses focus. An unavailable
        // cursor alone is not a leave: a menu the press opened hides the
        // pointer from this layer without it moving.
        let window_left = matches!(
            event,
            Event::Mouse(mouse::Event::CursorLeft) | Event::Window(window::Event::Unfocused)
        );
        let bounds = layout.bounds().intersection(viewport);
        if let Some(suppression) = &mut state.tip_suppressed
            && (window_left || suppression.released_by(cursor, bounds))
        {
            state.tip_suppressed = None;
        }
        if matches!(self.kind, Kind::Tooltip) && inside && state.inspect_control {
            // The Widget API has no enabled query. COSMIC buttons expose this
            // through their root a11y node. Inspect on entry/rebuild, not every
            // animation frame; noninteractive explanatory tooltips still work.
            let nodes = self
                .content
                .as_widget()
                .a11y_nodes(layout, &tree.children[0], cursor);
            state.disabled = nodes.root().len() == 1 && nodes.root()[0].node().is_disabled();
            state.inspect_control = false;
        }
        inside &= !state.disabled;
        if matches!(self.kind, Kind::Tooltip)
            && inside
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Touch(cosmic::iced::touch::Event::FingerPressed { .. })
            )
        {
            state.tip_suppressed = Some(PressSuppression {
                at: cursor.position(),
                obscured: false,
            });
            state.entered = None;
            state.tip_open = false;
            state.tooltip = Animation::new(false).duration(HOVER_DURATION);
            shell.invalidate_layout();
            shell.request_redraw();
        }

        if state.hover.value() != inside {
            state.hover.go_mut(inside, now);
            state.entered = inside.then_some(now);
            shell.request_redraw();
        }
        match self.kind {
            Kind::Hover { .. } if state.hover.is_animating(now) => shell.request_redraw(),
            Kind::Tooltip => {
                let mut visible = false;
                if let Some(entered) = state
                    .entered
                    .filter(|_| inside && state.tip_suppressed.is_none())
                {
                    let deadline = entered + TOOLTIP_DELAY;
                    visible = now >= deadline;
                    if !visible {
                        shell.request_redraw_at(deadline);
                    }
                }
                if state.tooltip.value() != visible {
                    state.tooltip.go_mut(visible, now);
                    if visible && !state.tip_open {
                        state.tip_open = true;
                        shell.invalidate_layout();
                    }
                    shell.request_redraw();
                }
                if state.tooltip.is_animating(now) {
                    shell.request_redraw();
                } else if !visible && state.tip_open {
                    state.tip_open = false;
                    shell.invalidate_layout();
                    shell.request_redraw();
                }
            }
            _ => {}
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
        if let Kind::RevealWidth { progress, .. } = self.kind {
            if progress < 1.0 || !cursor.is_over(layout.bounds()) {
                return mouse::Interaction::None;
            }
        }
        let interaction = self.content.as_widget().mouse_interaction(
            &tree.children[0],
            effect_layout(self.kind, layout),
            cursor,
            viewport,
            renderer,
        );
        if matches!(self.kind, Kind::Opaque)
            && interaction == mouse::Interaction::None
            && cursor.is_over(layout.bounds())
        {
            mouse::Interaction::Idle
        } else {
            interaction
        }
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
        if matches!(self.kind, Kind::RevealWidth { .. }) {
            if let Some(clipped) = layout.bounds().intersection(viewport) {
                renderer.with_layer(clipped, |renderer| {
                    self.content.as_widget().draw(
                        &tree.children[0],
                        renderer,
                        theme,
                        style,
                        effect_layout(self.kind, layout),
                        cursor,
                        &clipped,
                    );
                });
            }
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        if let Kind::Fade(progress) = self.kind {
            if progress <= 0.0 {
                return;
            }
            if progress < 1.0 {
                let mut cached = state.faded.borrow_mut();
                if cached.as_ref().is_none_or(|cached| cached.source != *theme) {
                    *cached = Some(FadedTheme::new(theme));
                }
                let faded = cached
                    .as_mut()
                    .expect("faded theme initialized")
                    .at(progress);
                let mut style = *style;
                style.text_color.a *= progress;
                style.icon_color.a *= progress;
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    faded,
                    &style,
                    layout,
                    cursor,
                    viewport,
                );
                return;
            }
        }
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if let Kind::Hover { themed_radius } = self.kind {
            // Keep low-emphasis rows legible against neutral surfaces without
            // turning hover feedback into a selected-state fill.
            let alpha = state.hover.interpolate(0.0, 0.10, state.now);
            if alpha > 0.0 {
                if let Some(bounds) = layout.bounds().intersection(viewport) {
                    let mut color: Color = theme.current_container().on.into();
                    color.a *= alpha;
                    // Draw after the child, otherwise opaque button surfaces
                    // completely cover the feedback. This captures no events.
                    renderer.with_layer(bounds, |renderer| {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: layout.bounds(),
                                border: cosmic::iced::Border {
                                    radius: if themed_radius {
                                        theme.cosmic().corner_radii.radius_s.into()
                                    } else {
                                        crate::design::RADIUS_S.into()
                                    },
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            color,
                        )
                    });
                }
            }
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
        if matches!(self.kind, Kind::RevealWidth { progress, .. } if progress < 1.0) {
            return None;
        }
        let tip_open = tree.state.downcast_ref::<State>().tip_open;
        if let Some(tip) = self.tip.as_mut().filter(|_| tip_open) {
            if tree.children.len() < 2 {
                tree.children.push(Tree::new(&*tip.content()));
            }
        }
        let (content_state, tip_state) = tree.children.split_at_mut(1);
        let child = self.content.as_widget_mut().overlay(
            &mut content_state[0],
            effect_layout(self.kind, layout),
            renderer,
            viewport,
            translation,
        );
        let state = tree.state.downcast_ref::<State>();
        if state.tip_open {
            if let Some(tip) = &mut self.tip {
                let prefer_above = tip.prefer_above;
                let tip = overlay::Element::new(Box::new(TipOverlay {
                    content: tip.content(),
                    tree: &mut tip_state[0],
                    anchor: layout.bounds() + translation,
                    progress: state.tooltip.interpolate(0.0, 1.0, state.now),
                    prefer_above,
                }));
                return Some(if let Some(child) = child {
                    overlay::Group::with_children(vec![child, tip]).overlay()
                } else {
                    tip
                });
            }
        }
        child
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(
            effect_layout(self.kind, layout),
            &tree.children[0],
            cursor,
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        if matches!(self.kind, Kind::RevealWidth { progress, .. } if progress < 1.0) {
            return;
        }
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            effect_layout(self.kind, layout),
            renderer,
            destinations,
        );
    }
}

impl<'a, Message: 'a> From<Effect<'a, Message>> for Element<'a, Message> {
    fn from(effect: Effect<'a, Message>) -> Self {
        Self::new(effect)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_menu_hints_use_effective_shortcut_configuration() {
        use cosmic::iced::keyboard::{Key, Modifiers};
        let chords = vec!["Ctrl+Alt+F5".to_owned(), "Ctrl+N".to_owned()];
        let bindings = menu_shortcuts([(chords.as_slice(), 7u8)]);
        assert_eq!(bindings.len(), 1);
        let (key, action) = bindings.iter().next().unwrap();
        assert_eq!(*action, 7);
        assert!(key.matches(
            Modifiers::CTRL | Modifiers::ALT,
            &Key::Named(cosmic::iced::keyboard::key::Named::F5),
            None
        ));
        assert!(!key.to_string().is_empty());
    }

    #[test]
    fn pressed_tooltip_stays_hidden_until_pointer_leaves() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut element = tooltip(
            cosmic::widget::button::text("Settings").on_press(1u8),
            "Open settings",
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        let bounds = Rectangle::with_size(Size::new(300.0, 100.0));
        let pointer = mouse::Cursor::Available(node.bounds().center());
        let state = tree.state.downcast_mut::<State>();
        state.tip_open = true;
        state.tooltip = Animation::new(true).duration(HOVER_DURATION);
        state.entered = Some(Instant::now() - TOOLTIP_DELAY);
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Window(window::Event::RedrawRequested(
                Instant::now() + TOOLTIP_DELAY,
            )),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                pointer,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
            assert!(!tree.state.downcast_ref::<State>().tip_open);
            assert!(tree.state.downcast_ref::<State>().tip_suppressed.is_some());
        }
        // A context menu the press opened covers the control: the runtime
        // hands this layer an unavailable cursor while the pointer stays put.
        // Closing the menu returns the same position, which is not a re-entry.
        for (event, cursor) in [
            (
                Event::Mouse(mouse::Event::CursorMoved {
                    position: node.bounds().center(),
                }),
                mouse::Cursor::Unavailable,
            ),
            (
                Event::Window(window::Event::RedrawRequested(
                    Instant::now() + TOOLTIP_DELAY,
                )),
                mouse::Cursor::Unavailable,
            ),
            (
                Event::Window(window::Event::RedrawRequested(
                    Instant::now() + TOOLTIP_DELAY * 2,
                )),
                pointer,
            ),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
            assert!(
                !tree.state.downcast_ref::<State>().tip_open,
                "a menu over the pressed control must not re-arm its tooltip"
            );
            assert!(tree.state.downcast_ref::<State>().tip_suppressed.is_some());
        }
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorLeft),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(tree.state.downcast_ref::<State>().tip_suppressed.is_none());
        for event in [
            Event::Mouse(mouse::Event::CursorMoved {
                position: node.bounds().center(),
            }),
            Event::Window(window::Event::RedrawRequested(
                Instant::now() + TOOLTIP_DELAY + Duration::from_millis(1),
            )),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                pointer,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        assert!(
            tree.state.downcast_ref::<State>().tip_open,
            "a fresh hover can show the tooltip again"
        );
    }

    #[test]
    fn press_suppression_compares_against_the_last_position_before_it_was_hidden() {
        let control = Rectangle::new(Point::new(10.0, 10.0), Size::new(100.0, 20.0));
        let pressed = Point::new(20.0, 20.0);
        let released = Point::new(21.0, 20.0);
        let mut suppression = PressSuppression {
            at: Some(pressed),
            obscured: false,
        };
        // The right button is released a pixel away, then its menu opens.
        assert!(!suppression.released_by(mouse::Cursor::Available(released), Some(control)));
        assert!(!suppression.released_by(mouse::Cursor::Unavailable, Some(control)));
        assert!(
            !suppression.released_by(mouse::Cursor::Available(released), Some(control)),
            "a menu closing under the unmoved pointer keeps the tooltip hidden"
        );
        assert!(!suppression.released_by(mouse::Cursor::Unavailable, Some(control)));
        assert!(
            suppression.released_by(
                mouse::Cursor::Available(Point::new(90.0, 20.0)),
                Some(control)
            ),
            "returning elsewhere on the control after being hidden is a re-entry"
        );
    }

    #[test]
    fn pressed_tooltip_returns_after_leaving_through_a_cursor_gate() {
        use cosmic::iced::advanced::clipboard;
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = tooltip(
            cosmic::widget::button::text("Settings").on_press(1u8),
            "Open settings",
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        let bounds = Rectangle::with_size(Size::new(300.0, 100.0));
        let control = node.bounds();
        let pressed = Point::new(control.x + 2.0, control.center_y());
        let returned = Point::new(control.x + control.width - 2.0, control.center_y());
        let start = Instant::now();
        let mut messages = Vec::new();
        // An ancestor that gates the cursor to its own region (a sidebar row
        // list, a scrollable) reports no position once the pointer leaves it,
        // so the control never sees a position outside itself.
        for (event, cursor) in [
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                mouse::Cursor::Available(pressed),
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(900.0, 900.0),
                }),
                mouse::Cursor::Unavailable,
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved { position: returned }),
                mouse::Cursor::Available(returned),
            ),
            (
                Event::Window(window::Event::RedrawRequested(start + TOOLTIP_DELAY * 2)),
                mouse::Cursor::Available(returned),
            ),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        let state = tree.state.downcast_ref::<State>();
        assert!(state.tip_suppressed.is_none());
        assert!(
            state.tip_open,
            "returning to the control after leaving its region shows the tooltip"
        );
    }

    /// A Work list row is a `button::custom` holding tooltip-bearing
    /// controls; the tooltip must open at the control, with the camera
    /// translation of an enclosing workspace card applied once.
    #[test]
    fn tooltips_inside_an_offset_button_anchor_to_their_control() {
        use cosmic::iced::advanced::clipboard;
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let screen = Size::new(600.0, 400.0);
        let control = Size::new(24.0, 24.0);
        let mut element: Element<'_, u8> = widget::container(
            widget::button::custom(
                widget::row::with_children(vec![
                    cosmic::iced::widget::Space::new()
                        .width(100)
                        .height(24)
                        .into(),
                    tooltip(
                        cosmic::iced::widget::Space::new()
                            .width(control.width)
                            .height(control.height),
                        "Card actions",
                    ),
                ])
                .spacing(8),
            )
            .padding(10)
            .on_press(1),
        )
        .padding(cosmic::iced::Padding {
            left: 140.0,
            top: 90.0,
            ..cosmic::iced::Padding::ZERO
        })
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, screen),
        );
        let anchor = Rectangle::new(Point::new(140.0 + 10.0 + 100.0 + 8.0, 90.0 + 10.0), control);
        let start = Instant::now();
        let viewport = Rectangle::with_size(screen);
        for event in [
            Event::Mouse(mouse::Event::CursorMoved {
                position: anchor.center(),
            }),
            Event::Window(window::Event::RedrawRequested(start + TOOLTIP_DELAY * 2)),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(anchor.center()),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut Vec::new()),
                &viewport,
            );
        }
        for camera in [Vector::ZERO, Vector::new(-60.0, 35.0)] {
            let mut overlay = element
                .as_widget_mut()
                .overlay(&mut tree, Layout::new(&node), &renderer, &viewport, camera)
                .expect("hovering past the delay opens the tooltip");
            // An overlay group spans the screen; the surface is its child.
            let tip = overlay
                .as_overlay_mut()
                .layout(&renderer, screen)
                .children()[0]
                .bounds();
            let anchor = anchor + camera;
            assert!(
                (tip.center_x() - anchor.center_x()).abs() < 0.5
                    && (tip.y - (anchor.y + anchor.height + TIP_GAP)).abs() < 0.5,
                "tooltip {tip:?} is not beneath its control {anchor:?}"
            );
        }
    }

    use super::*;
    #[test]
    fn an_exclusive_chooser_narrower_than_its_base_hides_the_base_from_the_pointer() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let bounds = Rectangle::with_size(Size::new(400.0, 200.0));
        // Only the gutter beside the narrower chooser lies over the base.
        let gutter = mouse::Cursor::Available(Point::new(390.0, 100.0));
        for (front, expected) in [
            (None, mouse::Interaction::Pointer),
            (
                Some(
                    widget::container(cosmic::iced::widget::Space::new())
                        .width(300)
                        .height(Length::Fill)
                        .into(),
                ),
                mouse::Interaction::None,
            ),
        ] {
            let mut element: Element<'_, u8> = exclusive_layer(
                widget::button::custom(cosmic::iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .on_press(1)
                    .into(),
                front,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, bounds.size()),
            );
            assert_eq!(
                element.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    gutter,
                    &bounds,
                    &renderer,
                ),
                expected
            );
        }
    }

    #[test]
    fn font_zoom_accepts_layout_variants_without_claiming_typing() {
        use cosmic::iced::keyboard::{
            Key, Modifiers,
            key::{Code, Physical},
        };
        let equal = Physical::Code(Code::Equal);
        assert_eq!(
            font_zoom_chord(&Key::Character("=".into()), equal, Modifiers::CTRL),
            Some(1)
        );
        assert_eq!(
            font_zoom_chord(
                &Key::Character("+".into()),
                equal,
                Modifiers::CTRL | Modifiers::SHIFT
            ),
            Some(1)
        );
        assert_eq!(
            font_zoom_chord(
                &Key::Character("-".into()),
                Physical::Code(Code::Minus),
                Modifiers::CTRL
            ),
            Some(-1)
        );
        assert_eq!(
            font_zoom_chord(
                &Key::Character("0".into()),
                Physical::Code(Code::Digit0),
                Modifiers::CTRL
            ),
            Some(0)
        );
        assert_eq!(
            font_zoom_chord(&Key::Character("=".into()), equal, Modifiers::empty()),
            None
        );
        assert_eq!(
            font_zoom_chord(
                &Key::Character("=".into()),
                equal,
                Modifiers::CTRL | Modifiers::ALT
            ),
            None
        );
    }

    #[test]
    fn focused_escape_answers_before_the_native_field_blurs() {
        use cosmic::iced::{
            advanced::{clipboard, renderer::Headless},
            keyboard,
        };
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let id = Id::from("escape.find");
        let mut element = focused_escape(
            crate::ui_controls::field::search("Find", "needle")
                .id(id.clone())
                .on_input(|_| 0_u8)
                .on_unfocus(2_u8),
            || 1_u8,
        );
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(Size::new(260.0, 100.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let event = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut published = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut published),
            &bounds,
        );
        assert!(
            published.is_empty(),
            "an unfocused find field cannot steal Escape"
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(id),
        );
        let mut shell = Shell::new(&mut published);
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &bounds,
        );
        assert!(shell.is_event_captured());
        assert_eq!(
            published,
            [1],
            "Escape closes find before COSMIC publishes blur"
        );
    }

    #[test]
    fn list_arrows_move_native_focus_and_enter_activates_only_the_destination() {
        use cosmic::iced::{
            advanced::{clipboard, renderer::Headless},
            keyboard,
        };
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let first = Id::from("list.first");
        let second = Id::from("list.second");
        let mut element = keyboard_list(
            widget::column::with_children(vec![
                keyboard_list_row(
                    crate::ui_controls::button::quiet("First")
                        .id(first.clone())
                        .on_press(1_u8),
                ),
                keyboard_list_row(
                    crate::ui_controls::button::quiet("Second")
                        .id(second)
                        .on_press(2_u8),
                ),
            ]),
            true,
        );
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(Size::new(260.0, 200.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(first),
        );
        let mut published = Vec::new();
        for key in [keyboard::key::Named::ArrowDown, keyboard::key::Named::Enter] {
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modified_key: keyboard::Key::Named(key),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut published),
                &bounds,
            );
        }
        assert_eq!(published, [2]);
    }

    #[test]
    fn filter_entry_keeps_editing_keys_and_down_enters_the_first_result() {
        use cosmic::iced::{
            advanced::{clipboard, renderer::Headless},
            keyboard,
        };
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let entry = Id::from("filter.entry");
        let mut element = keyboard_list(
            widget::column::with_children(vec![
                keyboard_list_entry(
                    crate::ui_controls::field::search("Filter", "long query")
                        .id(entry.clone())
                        .on_input(|_| 99_u8)
                        .on_submit(|_: String| 100_u8),
                ),
                keyboard_list_row(crate::ui_controls::button::quiet("First").on_press(1_u8)),
                keyboard_list_row(crate::ui_controls::button::quiet("Second").on_press(2_u8)),
            ]),
            true,
        );
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(Size::new(260.0, 240.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(entry),
        );
        let mut published = Vec::new();
        for key in [
            keyboard::key::Named::Home,
            keyboard::key::Named::End,
            keyboard::key::Named::ArrowUp,
            keyboard::key::Named::Enter,
            keyboard::key::Named::ArrowDown,
            keyboard::key::Named::Enter,
            keyboard::key::Named::ArrowDown,
            keyboard::key::Named::Enter,
        ] {
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modified_key: keyboard::Key::Named(key),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut published),
                &bounds,
            );
        }
        assert_eq!(
            published,
            [100, 1, 2],
            "editing keys keep search focus; Down moves to results"
        );
    }

    #[test]
    fn a_vertical_wheel_becomes_horizontal_pixels() {
        let Event::Mouse(mouse::Event::WheelScrolled { delta }) =
            wheel_as_horizontal(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 })
        else {
            panic!("expected a wheel event");
        };
        assert_eq!(delta, mouse::ScrollDelta::Pixels { x: 60.0, y: 0.0 });
    }

    #[test]
    fn a_wheel_scrolls_the_lane_under_the_pointer_and_moves_the_row_between_lanes() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

        /// Two 200-wide lanes, each a 40-high heading over a scrollable of
        /// 2000, in a 300-wide row that scrolls sideways by up to 120.
        fn view() -> Element<'static, ()> {
            let lane = || -> Element<'static, ()> {
                widget::column::with_children(vec![
                    widget::Space::new().width(Length::Fill).height(40).into(),
                    widget::scrollable(widget::Space::new().width(Length::Fill).height(2000))
                        .height(Length::Fill)
                        .into(),
                ])
                .width(200)
                .into()
            };
            horizontal_wheel_around_vertical(
                widget::scrollable::horizontal(
                    widget::row::with_children(vec![lane(), lane()])
                        .spacing(20)
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill),
            )
        }
        /// The row's sideways offset and each lane's offset.
        struct Offsets(Vec<Vector>);
        impl Operation for Offsets {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn scrollable(
                &mut self,
                _id: Option<&Id>,
                _bounds: Rectangle,
                _content_bounds: Rectangle,
                translation: Vector,
                _state: &mut dyn operation::Scrollable,
            ) {
                self.0.push(translation);
            }
        }
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        // Built and drawn once, as the runtime does before any input arrives:
        // a scrollable's first redraw records its viewport, and until then
        // iced takes the first wheel it sees as news and keeps it.
        let mut ui = || {
            let mut ui =
                UserInterface::build(view(), Size::new(300.0, 400.0), Cache::new(), &mut renderer);
            let _ = ui.update(
                &[Event::Window(
                    window::Event::RedrawRequested(Instant::now()),
                )],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            ui
        };
        let wheel = |ui: &mut UserInterface<'_, (), Theme, Renderer>,
                     x: f32,
                     y: f32,
                     delta: mouse::ScrollDelta| {
            let position = Point::new(x, y);
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position }),
                    Event::Mouse(mouse::Event::WheelScrolled { delta }),
                ],
                mouse::Cursor::Available(position),
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            let mut offsets = Offsets(Vec::new());
            ui.operate(&renderer, &mut offsets);
            let [row, first, second] = offsets.0[..] else {
                panic!("expected a row and two lanes: {:?}", offsets.0);
            };
            (row.x, first.y, second.y)
        };
        let notch = mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 };
        let pixels = |x, y| mouse::ScrollDelta::Pixels { x, y };

        // Over a lane the wheel scrolls that lane, and the row stays put.
        let mut over_lane = ui();
        assert_eq!(wheel(&mut over_lane, 100.0, 200.0, notch), (0.0, 60.0, 0.0));
        assert_eq!(
            wheel(&mut over_lane, 100.0, 200.0, notch),
            (0.0, 120.0, 0.0)
        );
        // So does a trackpad's vertical drag, whose small sideways jitter
        // must not hand the gesture to the row.
        assert_eq!(
            wheel(&mut over_lane, 100.0, 200.0, pixels(-1.5, -30.0)),
            (0.0, 150.0, 0.0)
        );
        // At the lane's end the wheel stays with the lane: the row does not
        // slide out from under the pointer. The lane scrolls 1640 at most.
        assert_eq!(
            wheel(&mut over_lane, 100.0, 200.0, pixels(0.0, -5000.0)),
            (0.0, 1640.0, 0.0)
        );
        assert_eq!(
            wheel(&mut over_lane, 100.0, 200.0, notch),
            (0.0, 1640.0, 0.0)
        );

        // A trackpad swipe that is mostly sideways moves the row, even over a
        // lane, and that lane keeps its place.
        let mut swiped = ui();
        assert_eq!(
            wheel(&mut swiped, 100.0, 200.0, pixels(-40.0, -10.0)),
            (40.0, 0.0, 0.0)
        );

        // Shift asks for sideways motion, as it does of iced's scrollables,
        // even over a lane.
        let mut shifted = ui();
        let shift = |ui: &mut UserInterface<'_, (), Theme, Renderer>, held: bool| {
            let modifiers = if held {
                cosmic::iced::keyboard::Modifiers::SHIFT
            } else {
                cosmic::iced::keyboard::Modifiers::empty()
            };
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let _ = ui.update(
                &[Event::Keyboard(
                    cosmic::iced::keyboard::Event::ModifiersChanged(modifiers),
                )],
                mouse::Cursor::Available(Point::new(100.0, 200.0)),
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
        };
        shift(&mut shifted, true);
        assert_eq!(wheel(&mut shifted, 100.0, 200.0, notch), (60.0, 0.0, 0.0));
        assert_eq!(wheel(&mut shifted, 100.0, 200.0, notch), (120.0, 0.0, 0.0));

        // Over a heading the wheel moves the row sideways, and goes on doing
        // so when that brings a lane under the pointer mid-gesture.
        let mut over_heading = ui();
        assert_eq!(
            wheel(&mut over_heading, 100.0, 20.0, notch),
            (60.0, 0.0, 0.0)
        );
        assert_eq!(
            wheel(&mut over_heading, 100.0, 200.0, notch),
            (120.0, 0.0, 0.0)
        );
    }

    #[test]
    fn a_trackpad_drag_goes_sideways_along_the_axis_it_mostly_moves() {
        let Event::Mouse(mouse::Event::WheelScrolled { delta }) =
            wheel_as_horizontal(mouse::ScrollDelta::Pixels { x: 0.5, y: 12.0 })
        else {
            panic!("expected a wheel event");
        };
        assert_eq!(delta, mouse::ScrollDelta::Pixels { x: 12.0, y: 0.0 });
    }

    #[test]
    fn a_diagonal_wheel_keeps_its_horizontal_component() {
        // Summing the axes cancelled this gesture to a standstill, and the
        // wrapper had already replaced the original event with the sum.
        for (delta, expected) in [
            (
                mouse::ScrollDelta::Lines { x: -1.0, y: 1.0 },
                mouse::ScrollDelta::Pixels { x: -60.0, y: 0.0 },
            ),
            (
                mouse::ScrollDelta::Pixels { x: -8.0, y: 8.0 },
                mouse::ScrollDelta::Pixels { x: -8.0, y: 0.0 },
            ),
            (
                mouse::ScrollDelta::Pixels { x: 0.0, y: 8.0 },
                mouse::ScrollDelta::Pixels { x: 8.0, y: 0.0 },
            ),
        ] {
            let Event::Mouse(mouse::Event::WheelScrolled { delta }) = wheel_as_horizontal(delta)
            else {
                panic!("expected a wheel event");
            };
            assert_eq!(delta, expected);
        }
    }

    #[test]
    fn overflow_edges_are_clear_when_content_fits() {
        assert_eq!(overflow_edges(0.0, 200.0, 180.0), (false, false));
        assert_eq!(overflow_edges(0.0, 200.0, 200.0), (false, false));
        assert_eq!(overflow_edges(0.25, 200.0, 200.4), (false, false));
    }

    #[test]
    fn overflow_edges_mark_whichever_side_can_still_scroll() {
        assert_eq!(overflow_edges(0.0, 200.0, 400.0), (false, true));
        assert_eq!(overflow_edges(12.0, 200.0, 400.0), (true, true));
        assert_eq!(overflow_edges(200.0, 200.0, 400.0), (true, false));
    }

    /// Whole frames of a real context menu, driven the way the runtime drives
    /// them: build (which diffs and lays out), then update (which builds the
    /// overlay, lays it out and feeds it the events). Menu state, overlays and
    /// reconciliation therefore interleave exactly as they do in the app.
    mod context_menus {
        use super::*;
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::event::Status;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum Act {
            Item(usize),
        }

        impl widget::menu::Action for Act {
            type Message = Act;
            fn message(&self) -> Self::Message {
                *self
            }
        }

        /// A flat menu of `items` buttons, the shape every menu in this app has.
        fn menu(items: usize) -> Vec<widget::menu::Tree<Act>> {
            let entries = (0..items)
                .map(|index| {
                    widget::menu::Item::Button(format!("Item {index}"), None, Act::Item(index))
                })
                .collect();
            widget::menu::items(&std::collections::HashMap::new(), entries)
        }

        fn row(items: usize) -> Element<'static, Act> {
            context_menu(bare_row(), menu(items))
        }

        fn bare_row() -> Element<'static, Act> {
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .height(ROW_HEIGHT)
                .into()
        }

        #[test]
        fn named_button_can_move_across_an_unnamed_spacer() {
            let row = |swapped: bool| -> Element<'static, Act> {
                let button: Element<'static, Act> = crate::ui_controls::button::secondary("Move")
                    .id(Id::new("test.moving.named-button"))
                    .on_press(Act::Item(7))
                    .into();
                let spacer: Element<'static, Act> = cosmic::iced::widget::Space::new()
                    .width(100)
                    .height(ROW_HEIGHT)
                    .into();
                widget::row::with_children(if swapped {
                    vec![spacer, button]
                } else {
                    vec![button, spacer]
                })
                .into()
            };
            let mut frames = Frames::new();
            for swapped in [false, true, false, true] {
                frames.frame(row(swapped), &[], ANCHOR);
            }
        }

        #[test]
        fn unchanged_lazy_named_buttons_keep_pressed_state_across_native_rebuilds() {
            let builds = std::rc::Rc::new(std::cell::Cell::new(0));
            let cached_button = || -> Element<'static, Act> {
                let builds = builds.clone();
                cosmic::iced::widget::lazy(0_u64, move |_| {
                    builds.set(builds.get() + 1);
                    // Exercise named descendants beneath both a named parent
                    // and unnamed decorators, as in cached project rows.
                    let content: Element<'static, Act> = widget::container(hover(
                        crate::ui_controls::button::secondary("Cached action")
                            .id(Id::new("test.cached.named-button"))
                            .on_press(Act::Item(7)),
                    ))
                    .id(Id::new("test.cached.named-container"))
                    .width(200)
                    .height(ROW_HEIGHT)
                    .into();
                    content
                })
                .into()
            };
            let mut frames = Frames::new();
            frames.frame(cached_button(), &[], ANCHOR);
            frames.frame(
                cached_button(),
                &[Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            frames.frame(
                cached_button(),
                &[Event::Mouse(mouse::Event::ButtonReleased(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            assert_eq!(frames.messages, vec![Act::Item(7)]);
            assert_eq!(
                builds.get(),
                1,
                "native rebuilds must restore state without rebuilding cached widgets"
            );
            // Removing the cached subtree must discard its named states.
            frames.frame(
                cached_button(),
                &[Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            frames.frame(bare_row(), &[], ANCHOR);
            frames.frame(
                cached_button(),
                &[Event::Mouse(mouse::Event::ButtonReleased(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            assert_eq!(
                frames.messages,
                vec![Act::Item(7)],
                "a new button must not inherit an old press"
            );
        }

        const ROW_HEIGHT: f32 = 32.0;

        #[test]
        fn reusing_a_named_button_id_for_stateless_content_discards_the_button_state() {
            let mut frames = Frames::new();
            let button = || -> Element<'static, Act> {
                widget::button::standard("Old action")
                    .id(Id::new("test.reused.named-control"))
                    .on_press(Act::Item(9))
                    .width(200)
                    .height(ROW_HEIGHT)
                    .into()
            };
            frames.frame(
                button(),
                &[Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            let replacement = || -> Element<'static, Act> {
                widget::container(widget::text("Replacement"))
                    .id(Id::new("test.reused.named-control"))
                    .width(200)
                    .height(ROW_HEIGHT)
                    .into()
            };
            frames.frame(replacement(), &[], ANCHOR);
            frames.frame(
                button(),
                &[Event::Mouse(mouse::Event::ButtonReleased(
                    mouse::Button::Left,
                ))],
                ANCHOR,
            );
            assert!(
                frames.messages.is_empty(),
                "a type change must not revive an old button press"
            );
        }
        const WINDOW: Size = Size {
            width: 400.0,
            height: 400.0,
        };
        /// Inside the first row, so a right click here opens that row's menu.
        const ANCHOR: Point = Point { x: 20.0, y: 16.0 };
        /// Inside the menu the anchor opens, so an open menu captures motion
        /// here and a dismissed one does not.
        const OVER_MENU: Point = Point { x: 60.0, y: 60.0 };

        fn headless_renderer() -> Renderer {
            use cosmic::iced::advanced::renderer::Headless;
            use cosmic::iced::{Font, Pixels};

            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("executor")
                .block_on(Renderer::new(
                    Font::default(),
                    Pixels(14.0),
                    Some("tiny-skia"),
                ))
                .expect("a headless renderer")
        }

        struct Frames {
            renderer: Renderer,
            cache: Cache,
            messages: Vec<Act>,
        }

        impl Frames {
            fn new() -> Self {
                Self {
                    renderer: headless_renderer(),
                    cache: Cache::new(),
                    messages: Vec::new(),
                }
            }

            fn frame(
                &mut self,
                element: Element<'static, Act>,
                events: &[Event],
                cursor: Point,
            ) -> Vec<Status> {
                let mut ui = UserInterface::build(
                    element,
                    WINDOW,
                    std::mem::replace(&mut self.cache, Cache::new()),
                    &mut self.renderer,
                );
                let (_, statuses) = ui.update(
                    events,
                    mouse::Cursor::Available(cursor),
                    &mut self.renderer,
                    &mut clipboard::Null,
                    &mut self.messages,
                );
                self.cache = ui.into_cache();
                statuses
            }

            /// Whether the menu anchored at [`ANCHOR`] is on screen, asked the
            /// way a user asks: only an open menu swallows the pointer.
            fn menu_is_open(&mut self, element: Element<'static, Act>) -> bool {
                self.frame(element, &[moved(OVER_MENU)], OVER_MENU)
                    .contains(&Status::Captured)
            }

            /// Mounts the row, right-clicks it and lets the requested frame
            /// arrive, which is when the native widget measures the menu.
            fn open_menu(&mut self, items: usize) {
                let _ = self.frame(row(items), &[], ANCHOR);
                let _ = self.frame(row(items), &right_click(), ANCHOR);
                let _ = self.frame(row(items), &[redraw()], ANCHOR);
                assert!(
                    self.menu_is_open(row(items)),
                    "a right click on the row should have opened its {items}-item menu"
                );
            }
        }

        fn moved(position: Point) -> Event {
            Event::Mouse(mouse::Event::CursorMoved { position })
        }

        /// The frame the opening right click asks for. The native widget only
        /// measures its menu while the pointer is still over the row, so this
        /// is the event that puts the menu on screen.
        fn redraw() -> Event {
            Event::Window(window::Event::RedrawRequested(Instant::now()))
        }

        fn right_click() -> Vec<Event> {
            vec![
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
            ]
        }

        /// An open menu keeps the motions over it from everything beneath it,
        /// and the window's input trace records them all the same.
        #[test]
        fn an_open_menu_does_not_hide_motion_from_the_input_trace() {
            use crate::input_trace::{InputTrace, observe};

            let input = InputTrace::default();
            let traced = || observe(row(3), &input);
            let mut frames = Frames::new();
            let _ = frames.frame(traced(), &[moved(ANCHOR)], ANCHOR);
            let _ = frames.frame(traced(), &right_click(), ANCHOR);
            let _ = frames.frame(traced(), &[redraw()], ANCHOR);
            let further = Point::new(OVER_MENU.x + 1.0, OVER_MENU.y);
            let statuses = frames.frame(traced(), &[moved(OVER_MENU), moved(further)], further);
            assert_eq!(statuses, [Status::Captured, Status::Captured]);
            assert_eq!(input.previous_position(), Some(OVER_MENU));
        }

        #[test]
        fn a_single_item_context_menu_activates_its_visible_item() {
            for move_before_press in [false, true] {
                let mut frames = Frames::new();
                // The popup stays inside the window even when the clicked
                // row fills its width; this point is inside its visible item.
                let point = Point::new(220.0, 32.0);
                let _ = frames.frame(row(1), &[], ANCHOR);
                let _ = frames.frame(row(1), &right_click(), ANCHOR);
                let _ = frames.frame(row(1), &[redraw()], ANCHOR);
                if move_before_press {
                    let _ = frames.frame(row(1), &[moved(point)], point);
                }
                let _ = frames.frame(
                    row(1),
                    &[Event::Mouse(mouse::Event::ButtonPressed(
                        mouse::Button::Left,
                    ))],
                    point,
                );
                let _ = frames.frame(
                    row(1),
                    &[Event::Mouse(mouse::Event::ButtonReleased(
                        mouse::Button::Left,
                    ))],
                    point,
                );
                assert_eq!(frames.messages, vec![Act::Item(0)]);
            }
        }

        #[test]
        fn a_menu_initializes_before_the_pointer_leaves_its_anchor() {
            let mut frames = Frames::new();
            frames.frame(row(3), &[], ANCHOR);
            frames.frame(row(3), &right_click(), ANCHOR);
            // Cursor motion may arrive before the requested redraw. The menu
            // must measure at the opening click before handling that motion.
            frames.frame(row(3), &[moved(OVER_MENU)], OVER_MENU);
            assert!(frames.menu_is_open(row(3)));
        }

        #[test]
        fn an_open_menu_outlives_frames_that_leave_its_items_alone() {
            // The dismissal below must cost something, or it would be no fix at
            // all: a menu that closes on the next frame regardless is useless.
            let mut frames = Frames::new();
            frames.open_menu(6);
            for _ in 0..3 {
                assert!(
                    frames.menu_is_open(row(6)),
                    "an unchanged menu should stay open across frames"
                );
            }
        }

        #[test]
        fn an_open_menu_losing_items_dismisses_itself() {
            // The native widget measures a position and a size per item once,
            // when the menu opens, and then reads that geometry against every
            // later frame's item list. Shrinking the list used to leave the
            // overlay indexing the new list by the old geometry.
            let mut frames = Frames::new();
            frames.open_menu(6);
            assert!(
                !frames.menu_is_open(row(4)),
                "a menu missing two of its items is not the menu that was opened"
            );
            for items in [3, 1] {
                assert!(!frames.menu_is_open(row(items)));
            }
        }

        #[test]
        fn an_open_menu_gaining_items_dismisses_itself() {
            let mut frames = Frames::new();
            frames.open_menu(2);
            assert!(!frames.menu_is_open(row(9)));
        }

        #[test]
        fn a_row_that_loses_its_menu_entirely_can_get_it_back() {
            // Sidebar and file rows drop the whole wrapper when a modal opens
            // or the host forbids the commands, so the menu leaves and returns.
            let mut frames = Frames::new();
            frames.open_menu(5);
            assert!(!frames.menu_is_open(bare_row()));
            let _ = frames.frame(row(5), &right_click(), ANCHOR);
            let _ = frames.frame(row(5), &[redraw()], ANCHOR);
            assert!(
                frames.menu_is_open(row(5)),
                "the row should open a fresh menu once its wrapper is back"
            );
        }

        #[test]
        fn a_dismissed_menu_leaves_the_row_able_to_open_another() {
            let mut frames = Frames::new();
            frames.open_menu(6);
            assert!(!frames.menu_is_open(row(3)));
            let _ = frames.frame(row(3), &right_click(), ANCHOR);
            let _ = frames.frame(row(3), &[redraw()], ANCHOR);
            assert!(
                frames.menu_is_open(row(3)),
                "the surface should still open the menu it now has"
            );
        }

        /// `rows` rows in a scrollable, the one at `anchored` carrying `items`.
        fn scrolled(rows: usize, anchored: usize, items: usize) -> Element<'static, Act> {
            let children = (0..rows)
                .map(|index| {
                    if index == anchored {
                        row(items)
                    } else {
                        row(items + 1)
                    }
                })
                .collect::<Vec<_>>();
            widget::scrollable(widget::column::with_children(children))
                .height(Length::Fixed(WINDOW.height))
                .into()
        }

        #[test]
        fn a_menu_in_a_scrollable_survives_its_row_moving_and_leaving() {
            // A row can scroll away, or be replaced at its index by a row whose
            // menu is a different length, while its menu is still up.
            let mut frames = Frames::new();
            let _ = frames.frame(scrolled(40, 0, 5), &[], ANCHOR);
            let _ = frames.frame(scrolled(40, 0, 5), &right_click(), ANCHOR);
            let _ = frames.frame(scrolled(40, 0, 5), &[redraw()], ANCHOR);
            for events in [
                vec![Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -240.0 },
                })],
                vec![moved(OVER_MENU)],
            ] {
                let _ = frames.frame(scrolled(40, 0, 5), &events, OVER_MENU);
            }
            // The anchored row's menu grows, then the row is gone altogether.
            let _ = frames.frame(scrolled(40, 0, 8), &[moved(OVER_MENU)], OVER_MENU);
            let _ = frames.frame(scrolled(3, 2, 8), &[moved(OVER_MENU)], OVER_MENU);
            assert!(
                !frames.menu_is_open(scrolled(3, 9, 5)),
                "no menu should survive its row being replaced"
            );
        }
    }
}

// Keep the wrapper anonymous: giving it its child's ID would register two
// incompatible states under one name in iced's named-widget reconciliation.
fn control_identity(tree: &Tree) -> Option<&Id> {
    tree.id
        .as_ref()
        .or_else(|| tree.children.first().and_then(control_identity))
}

struct TipOverlay<'a, 'b, Message> {
    content: &'a mut Element<'b, Message>,
    tree: &'a mut Tree,
    anchor: Rectangle,
    progress: f32,
    prefer_above: bool,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for TipOverlay<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.content.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = node.size();
        let x = (self.anchor.center_x() - size.width / 2.0)
            .clamp(0.0, (bounds.width - size.width).max(0.0));
        let below = self.anchor.y + self.anchor.height + TIP_GAP;
        let y = if !self.prefer_above && below + size.height <= bounds.height {
            below
        } else {
            self.anchor.y - size.height - TIP_GAP
        };
        node.move_to(Point::new(
            x,
            y.clamp(0.0, (bounds.height - size.height).max(0.0)),
        ))
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        if self.progress <= 0.0 {
            return;
        }
        // The same surface as every other floating element
        // (`design::elevated`), faded as two primitives rather than by
        // deriving and serializing an entire theme per frame.
        let component = &theme.current_container().component;
        let fade = |color: Color| Color {
            a: color.a * self.progress,
            ..color
        };
        let mut style = *style;
        style.text_color = fade(component.on.into());
        renderer.with_layer(layout.bounds(), |renderer| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: cosmic::iced::Border {
                        color: fade(crate::design::component_rule(theme)),
                        width: crate::design::BORDER_WIDTH,
                        radius: crate::design::RADIUS_S.into(),
                    },
                    ..Default::default()
                },
                fade(component.base.into()),
            );
            self.content.as_widget().draw(
                self.tree,
                renderer,
                theme,
                &style,
                layout,
                cursor,
                &layout.bounds(),
            );
        });
    }
}

struct FadedTheme {
    source: Theme,
    serialized: serde_json::Value,
    progress: f32,
    theme: Theme,
}

impl FadedTheme {
    fn new(source: &Theme) -> Self {
        Self {
            source: source.clone(),
            serialized: serde_json::to_value(source.cosmic())
                .expect("pinned COSMIC theme is serializable"),
            progress: 1.0,
            theme: source.clone(),
        }
    }

    fn at(&mut self, progress: f32) -> &Theme {
        if self.progress == progress {
            return &self.theme;
        }
        let source = self.source.cosmic();
        // Only the six surface containers are private in cosmic_theme::Theme.
        // Cache the source serialization, overwrite their colors from typed
        // originals, and deserialize by reference (no per-frame JSON clone).
        // ColorRepr explicitly accepts palette's floating-point RGBA variant;
        // hex serialization would quantize alpha to 8 bits on every frame.
        for (key, container) in [
            ("background", source.background(false)),
            ("primary", source.primary(false)),
            ("secondary", source.secondary(false)),
            ("transparent_background", source.background(true)),
            ("transparent_primary", source.primary(true)),
            ("transparent_secondary", source.secondary(true)),
        ] {
            set_container_colors(&mut self.serialized[key], container, progress);
        }
        let mut cosmic = cosmic::cosmic_theme::Theme::deserialize(&self.serialized)
            .expect("typed RGBA colors preserve the pinned COSMIC theme schema");
        // Public semantic colors can be adjusted directly without serializing,
        // parsing, or rounding their RGB components or original transparency.
        macro_rules! components {
            ($($field:ident),+ $(,)?) => {$(
                cosmic.$field = source.$field.clone();
                for color in component_colors(&mut cosmic.$field) { color.alpha *= progress; }
            )+};
        }
        components!(
            button,
            accent,
            success,
            destructive,
            warning,
            accent_button,
            success_button,
            destructive_button,
            warning_button,
            icon_button,
            link_button,
            list_button,
            text_button
        );
        cosmic.palette = source.palette.clone();
        macro_rules! palette_colors {
            ($($field:ident),+ $(,)?) => {$(cosmic.palette.$field.alpha *= progress;)+};
        }
        palette_colors!(
            bright_red,
            bright_green,
            bright_orange,
            gray_1,
            gray_2,
            neutral_0,
            neutral_1,
            neutral_2,
            neutral_3,
            neutral_4,
            neutral_5,
            neutral_6,
            neutral_7,
            neutral_8,
            neutral_9,
            neutral_10,
            accent_blue,
            accent_indigo,
            accent_purple,
            accent_pink,
            accent_red,
            accent_orange,
            accent_yellow,
            accent_green,
            accent_warm_grey,
            ext_warm_grey,
            ext_orange,
            ext_yellow,
            ext_blue,
            ext_purple,
            ext_pink,
            ext_indigo
        );
        cosmic.shade = source.shade;
        cosmic.shade.alpha *= progress;
        cosmic.accent_text = source.accent_text.map(|mut color| {
            color.alpha *= progress;
            color
        });
        cosmic.control_tint = source.control_tint;
        cosmic.text_tint = source.text_tint;
        self.theme = Theme {
            theme_type: cosmic::theme::ThemeType::Custom(Arc::new(cosmic)),
            ..self.source.clone()
        };
        self.progress = progress;
        &self.theme
    }
}

fn component_colors(component: &mut Component) -> [&mut Srgba; 12] {
    [
        &mut component.base,
        &mut component.hover,
        &mut component.pressed,
        &mut component.selected,
        &mut component.selected_text,
        &mut component.focus,
        &mut component.divider,
        &mut component.on,
        &mut component.disabled,
        &mut component.on_disabled,
        &mut component.border,
        &mut component.disabled_border,
    ]
}

fn set_container_colors(value: &mut serde_json::Value, source: &Container, progress: f32) {
    for (key, color) in [
        ("base", source.base),
        ("divider", source.divider),
        ("on", source.on),
        ("small_widget", source.small_widget),
    ] {
        set_color(&mut value[key], color, progress);
    }
    let component = &source.component;
    for (key, color) in [
        ("base", component.base),
        ("hover", component.hover),
        ("pressed", component.pressed),
        ("selected", component.selected),
        ("selected_text", component.selected_text),
        ("focus", component.focus),
        ("divider", component.divider),
        ("on", component.on),
        ("disabled", component.disabled),
        ("on_disabled", component.on_disabled),
        ("border", component.border),
        ("disabled_border", component.disabled_border),
    ] {
        set_color(&mut value["component"][key], color, progress);
    }
}

fn set_color(value: &mut serde_json::Value, source: Srgba, progress: f32) {
    if !value.is_object() {
        // Serialize the typed RGBA alternative once, not the hex-based theme
        // serializer. Subsequent frames replace only its numeric alpha.
        *value = serde_json::to_value(source).expect("palette RGBA is serializable");
    }
    value["alpha"] = (source.alpha * progress).into();
}
