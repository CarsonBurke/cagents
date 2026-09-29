//! Implement your own event loop to drive a user interface.

use iced_core::clipboard::DndDestinationRectangles;
use iced_core::widget::tree::NAMED;

use crate::core::event::{self, Event};
use crate::core::layout;
use crate::core::mouse;
use crate::core::overlay;
use crate::core::renderer;
use crate::core::widget;
use crate::core::window;
use crate::core::{
    Clipboard, Element, InputMethod, Layout, Rectangle, Shell, Size, Vector,
};

/// A set of interactive graphical elements with a specific [`Layout`].
///
/// It can be updated and drawn.
///
/// Iced tries to avoid dictating how to write your event loop. You are in
/// charge of using this type in your system in any way you want.
///
/// # Example
/// The [`integration`] example uses a [`UserInterface`] to integrate Iced in an
/// existing graphical application.
///
/// [`integration`]: https://github.com/iced-rs/iced/tree/0.14/examples/integration
pub struct UserInterface<'a, Message, Theme, Renderer> {
    root: Element<'a, Message, Theme, Renderer>,
    base: layout::Node,
    state: widget::Tree,
    overlay: Option<Overlay>,
    bounds: Size,
    cursor: Option<mouse::Cursor>,
}

struct Overlay {
    layout: layout::Node,
    interaction: mouse::Interaction,
}

// A cached/lazy widget can skip child diffing even though the runtime extracted
// all its named descendants. Reclaim only surviving State::None placeholders;
// ordinary reconciliation already consumed the states for rebuilt widgets.
/// Restores extracted named states in retained cached widget subtrees.
///
/// Custom event loops that use `Tree::take_all_named` must call this after
/// reconciliation and before layout, just as [`UserInterface::build`] does.
pub fn restore_cached_named_states(tree: &mut widget::Tree) {
    if matches!(tree.state, widget::tree::State::None)
        && let Some(crate::core::id::Id(crate::core::id::Internal::Custom(_, name))) = &tree.id
        && let Some((state, children)) = NAMED.with(|named| named.borrow_mut().remove(name))
    {
        // A rebuilt stateless widget may reuse a removed stateful widget's
        // name. Its State::None is fresh rather than an extracted placeholder.
        // Do not install the old widget's state or child shape into it.
        if tree.tag != widget::tree::Tag::stateless()
            || matches!(state, widget::tree::State::None)
        {
            tree.state = state;
            for (index, child) in children {
                if let Some(slot) = tree.children.get_mut(index) {
                    *slot = child;
                }
            }
        }
    }
    for child in &mut tree.children {
        restore_cached_named_states(child);
    }
}

impl<'a, Message, Theme, Renderer> UserInterface<'a, Message, Theme, Renderer>
where
    Renderer: crate::core::Renderer,
{
    /// Builds a user interface for an [`Element`].
    ///
    /// It is able to avoid expensive computations when using a [`Cache`]
    /// obtained from a previous instance of a [`UserInterface`].
    ///
    /// # Example
    /// Imagine we want to build a [`UserInterface`] for
    /// [the counter example that we previously wrote](index.html#usage). Here
    /// is naive way to set up our application loop:
    ///
    /// ```no_run
    /// # mod iced_wgpu {
    /// #     pub type Renderer = ();
    /// # }
    /// #
    /// # pub struct Counter;
    /// #
    /// # impl Counter {
    /// #     pub fn new() -> Self { Counter }
    /// #     pub fn view(&self) -> iced_core::Element<(), (), Renderer> { unimplemented!() }
    /// #     pub fn update(&mut self, _: ()) {}
    /// # }
    /// use iced_runtime::core::Size;
    /// use iced_runtime::user_interface::{self, UserInterface};
    /// use iced_wgpu::Renderer;
    ///
    /// // Initialization
    /// let mut counter = Counter::new();
    /// let mut cache = user_interface::Cache::new();
    /// let mut renderer = Renderer::default();
    /// let mut window_size = Size::new(1024.0, 768.0);
    ///
    /// // Application loop
    /// loop {
    ///     // Process system events here...
    ///
    ///     // Build the user interface
    ///     let user_interface = UserInterface::build(
    ///         counter.view(),
    ///         window_size,
    ///         cache,
    ///         &mut renderer,
    ///     );
    ///
    ///     // Update and draw the user interface here...
    ///     // ...
    ///
    ///     // Obtain the cache for the next iteration
    ///     cache = user_interface.into_cache();
    /// }
    /// ```
    pub fn build<E: Into<Element<'a, Message, Theme, Renderer>>>(
        root: E,
        bounds: Size,
        cache: Cache,
        renderer: &mut Renderer,
    ) -> Self {
        let mut root = root.into();

        let Cache { mut state, cursor } = cache;
        NAMED.with(|named| {
            let mut guard = named.borrow_mut();
            *guard = state.take_all_named();
        });

        state.diff(root.as_widget_mut());

        // Lazy widgets can retain their exact child widgets and skip child
        // diffing. Restore their extracted named states before layout.
        // States belonging to removed widgets are still discarded below.
        restore_cached_named_states(&mut state);

        let base = root.as_widget_mut().layout(
            &mut state,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );

        NAMED.with(|named| {
            named.borrow_mut().clear();
        });

        UserInterface {
            root,
            base,
            state,
            overlay: None,
            bounds,
            cursor,
        }
    }

    /// Updates the [`UserInterface`] by processing each provided [`Event`].
    ///
    /// It returns __messages__ that may have been produced as a result of user
    /// interactions. You should feed these to your __update logic__.
    ///
    /// # Example
    /// Let's allow our [counter](index.html#usage) to change state by
    /// completing [the previous example](#example):
    ///
    /// ```no_run
    /// # mod iced_wgpu {
    /// #     pub type Renderer = ();
    /// # }
    /// #
    /// # pub struct Counter;
    /// #
    /// # impl Counter {
    /// #     pub fn new() -> Self { Counter }
    /// #     pub fn view(&self) -> iced_core::Element<(), (), Renderer> { unimplemented!() }
    /// #     pub fn update(&mut self, _: ()) {}
    /// # }
    /// use iced_runtime::core::clipboard;
    /// use iced_runtime::core::mouse;
    /// use iced_runtime::core::Size;
    /// use iced_runtime::user_interface::{self, UserInterface};
    /// use iced_wgpu::Renderer;
    ///
    /// let mut counter = Counter::new();
    /// let mut cache = user_interface::Cache::new();
    /// let mut renderer = Renderer::default();
    /// let mut window_size = Size::new(1024.0, 768.0);
    /// let mut cursor = mouse::Cursor::default();
    /// let mut clipboard = clipboard::Null;
    ///
    /// // Initialize our event storage
    /// let mut events = Vec::new();
    /// let mut messages = Vec::new();
    ///
    /// loop {
    ///     // Obtain system events...
    ///
    ///     let mut user_interface = UserInterface::build(
    ///         counter.view(),
    ///         window_size,
    ///         cache,
    ///         &mut renderer,
    ///     );
    ///
    ///     // Update the user interface
    ///     let (state, event_statuses) = user_interface.update(
    ///         &events,
    ///         cursor,
    ///         &mut renderer,
    ///         &mut clipboard,
    ///         &mut messages
    ///     );
    ///
    ///     cache = user_interface.into_cache();
    ///
    ///     // Process the produced messages
    ///     for message in messages.drain(..) {
    ///         counter.update(message);
    ///     }
    /// }
    /// ```
    pub fn update(
        &mut self,
        events: &[Event],
        cursor: mouse::Cursor,
        renderer: &mut Renderer,
        clipboard: &mut dyn Clipboard,
        messages: &mut Vec<Message>,
    ) -> (State, Vec<event::Status>) {
        // The supplied cursor is the position at the END of this batch.
        // A later motion must not change the target of an earlier press.
        let has_motion = events.iter().any(|event| {
            matches!(
                event,
                Event::Mouse(
                    mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft
                )
            )
        });
        let mut event_cursor =
            if has_motion || self.cursor == Some(mouse::Cursor::Unavailable) {
                self.cursor.unwrap_or(mouse::Cursor::Unavailable)
            } else {
                cursor
            };
        let event_cursors: Vec<_> = events
            .iter()
            .map(|event| {
                match event {
                    Event::Mouse(mouse::Event::CursorMoved { position }) => {
                        event_cursor = mouse::Cursor::Available(*position);
                    }
                    Event::Mouse(mouse::Event::CursorLeft) => {
                        event_cursor = mouse::Cursor::Unavailable;
                    }
                    Event::Mouse(mouse::Event::CursorEntered) if !has_motion => {
                        // Enter has no position of its own. The supplied
                        // position is safe only when no later motion/leave
                        // can have changed it within this batch.
                        event_cursor = cursor;
                    }
                    _ => {}
                }
                event_cursor
            })
            .collect();
        // Redraws can run before the queued input is drained and already carry
        // its final position. They must not overwrite the last INPUT position.
        // Keep an explicit leave unavailable until another motion/enter arrives:
        // some backends retain their last position after CursorLeft.
        if events.iter().any(|event| matches!(event, Event::Mouse(_))) {
            self.cursor = Some(event_cursor);
        }
        let mut base_cursors = event_cursors.clone();
        let mut outdated = false;
        let mut redraw_request = window::RedrawRequest::Wait;
        let mut input_method = InputMethod::Disabled;
        let mut has_layout_changed = false;
        let viewport = Rectangle::with_size(self.bounds);

        let mut maybe_overlay = self
            .root
            .as_widget_mut()
            .overlay(
                &mut self.state,
                Layout::new(&self.base),
                renderer,
                &viewport,
                Vector::ZERO,
            )
            .map(overlay::Nested::new);

        let (base_cursor, overlay_statuses, overlay_interaction) =
            if maybe_overlay.is_some() {
                let bounds = self.bounds;

                let mut overlay = maybe_overlay.as_mut().unwrap();
                let mut layout = overlay.layout(renderer, bounds);
                let mut event_statuses = Vec::new();

                for (index, (event, event_cursor)) in
                    events.iter().zip(&event_cursors).enumerate()
                {
                    let mut shell = Shell::new(messages);

                    // Overlay hit testing must use this event's position too.
                    if overlay.mouse_interaction(
                        Layout::new(&layout),
                        *event_cursor,
                        renderer,
                    ) != mouse::Interaction::None
                    {
                        base_cursors[index] = mouse::Cursor::Unavailable;
                    }
                    overlay.update(
                        event,
                        Layout::new(&layout),
                        *event_cursor,
                        renderer,
                        clipboard,
                        &mut shell,
                    );

                    event_statuses.push(shell.event_status());
                    redraw_request = redraw_request.min(shell.redraw_request());
                    input_method.merge(shell.input_method());

                    if shell.is_layout_invalid() {
                        drop(maybe_overlay);

                        self.base = self.root.as_widget_mut().layout(
                            &mut self.state,
                            renderer,
                            &layout::Limits::new(Size::ZERO, self.bounds),
                        );

                        maybe_overlay = self
                            .root
                            .as_widget_mut()
                            .overlay(
                                &mut self.state,
                                Layout::new(&self.base),
                                renderer,
                                &viewport,
                                Vector::ZERO,
                            )
                            .map(overlay::Nested::new);

                        if maybe_overlay.is_none() {
                            break;
                        }

                        overlay = maybe_overlay.as_mut().unwrap();

                        shell.revalidate_layout(|| {
                            layout = overlay.layout(renderer, bounds);
                            has_layout_changed = true;
                        });
                    }

                    if shell.are_widgets_invalid() {
                        outdated = true;
                    }
                }

                let (base_cursor, interaction) =
                    if let Some(overlay) = maybe_overlay.as_mut() {
                        let interaction = cursor
                            .position()
                            .map(|cursor_position| {
                                overlay.mouse_interaction(
                                    Layout::new(&layout),
                                    mouse::Cursor::Available(cursor_position),
                                    renderer,
                                )
                            })
                            .unwrap_or_default();

                        if interaction == mouse::Interaction::None {
                            (cursor, mouse::Interaction::None)
                        } else {
                            (mouse::Cursor::Unavailable, interaction)
                        }
                    } else {
                        (cursor, mouse::Interaction::None)
                    };

                self.overlay = Some(Overlay {
                    layout,
                    interaction,
                });

                (base_cursor, event_statuses, interaction)
            } else {
                (
                    cursor,
                    vec![event::Status::Ignored; events.len()],
                    mouse::Interaction::None,
                )
            };

        drop(maybe_overlay);

        let event_statuses = events
            .iter()
            .zip(overlay_statuses)
            .zip(base_cursors)
            .map(|((event, overlay_status), event_cursor)| {
                if matches!(overlay_status, event::Status::Captured) {
                    return overlay_status;
                }

                let mut shell = Shell::new(messages);

                self.root.as_widget_mut().update(
                    &mut self.state,
                    event,
                    Layout::new(&self.base),
                    event_cursor,
                    renderer,
                    clipboard,
                    &mut shell,
                    &viewport,
                );

                if shell.event_status() == event::Status::Captured {
                    self.overlay = None;
                }

                redraw_request = redraw_request.min(shell.redraw_request());
                input_method.merge(shell.input_method());

                shell.revalidate_layout(|| {
                    has_layout_changed = true;

                    self.base = self.root.as_widget_mut().layout(
                        &mut self.state,
                        renderer,
                        &layout::Limits::new(Size::ZERO, self.bounds),
                    );

                    if let Some(mut overlay) = self
                        .root
                        .as_widget_mut()
                        .overlay(
                            &mut self.state,
                            Layout::new(&self.base),
                            renderer,
                            &viewport,
                            Vector::ZERO,
                        )
                        .map(overlay::Nested::new)
                    {
                        let layout = overlay.layout(renderer, self.bounds);
                        let interaction = overlay.mouse_interaction(
                            Layout::new(&layout),
                            cursor,
                            renderer,
                        );

                        self.overlay = Some(Overlay {
                            layout,
                            interaction,
                        });
                    }
                });

                if shell.are_widgets_invalid() {
                    outdated = true;
                }

                shell.event_status().merge(overlay_status)
            })
            .collect();

        let mouse_interaction =
            if overlay_interaction == mouse::Interaction::None {
                self.root.as_widget().mouse_interaction(
                    &self.state,
                    Layout::new(&self.base),
                    base_cursor,
                    &viewport,
                    renderer,
                )
            } else {
                overlay_interaction
            };

        (
            if outdated {
                State::Outdated
            } else {
                State::Updated {
                    mouse_interaction,
                    redraw_request,
                    input_method,
                    has_layout_changed,
                }
            },
            event_statuses,
        )
    }

    /// Draws the [`UserInterface`] with the provided [`Renderer`].
    ///
    /// It returns the current [`mouse::Interaction`]. You should update the
    /// icon of the mouse cursor accordingly in your system.
    ///
    /// [`Renderer`]: crate::core::Renderer
    ///
    /// # Example
    /// We can finally draw our [counter](index.html#usage) by
    /// [completing the last example](#example-1):
    ///
    /// ```no_run
    /// # mod iced_wgpu {
    /// #     pub type Renderer = ();
    /// #     pub type Theme = ();
    /// # }
    /// #
    /// # pub struct Counter;
    /// #
    /// # impl Counter {
    /// #     pub fn new() -> Self { Counter }
    /// #     pub fn view(&self) -> Element<(), (), Renderer> { unimplemented!() }
    /// #     pub fn update(&mut self, _: ()) {}
    /// # }
    /// use iced_runtime::core::clipboard;
    /// use iced_runtime::core::mouse;
    /// use iced_runtime::core::renderer;
    /// use iced_runtime::core::{Element, Size};
    /// use iced_runtime::user_interface::{self, UserInterface};
    /// use iced_wgpu::{Renderer, Theme};
    ///
    /// let mut counter = Counter::new();
    /// let mut cache = user_interface::Cache::new();
    /// let mut renderer = Renderer::default();
    /// let mut window_size = Size::new(1024.0, 768.0);
    /// let mut cursor = mouse::Cursor::default();
    /// let mut clipboard = clipboard::Null;
    /// let mut events = Vec::new();
    /// let mut messages = Vec::new();
    /// let mut theme = Theme::default();
    ///
    /// loop {
    ///     // Obtain system events...
    ///
    ///     let mut user_interface = UserInterface::build(
    ///         counter.view(),
    ///         window_size,
    ///         cache,
    ///         &mut renderer,
    ///     );
    ///
    ///     // Update the user interface
    ///     let event_statuses = user_interface.update(
    ///         &events,
    ///         cursor,
    ///         &mut renderer,
    ///         &mut clipboard,
    ///         &mut messages
    ///     );
    ///
    ///     // Draw the user interface
    ///     let mouse_interaction = user_interface.draw(&mut renderer, &theme, &renderer::Style::default(), cursor);
    ///
    ///     cache = user_interface.into_cache();
    ///
    ///     for message in messages.drain(..) {
    ///         counter.update(message);
    ///     }
    ///
    ///     // Update mouse cursor icon...
    ///     // Flush rendering operations...
    /// }
    /// ```
    pub fn draw(
        &mut self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        cursor: mouse::Cursor,
    ) {
        let viewport = Rectangle::with_size(self.bounds);
        renderer.reset(viewport);

        let base_cursor = match &self.overlay {
            None
            | Some(Overlay {
                interaction: mouse::Interaction::None,
                ..
            }) => cursor,
            _ => mouse::Cursor::Unavailable,
        };

        self.root.as_widget().draw(
            &self.state,
            renderer,
            theme,
            style,
            Layout::new(&self.base),
            base_cursor,
            &viewport,
        );

        let Self {
            overlay,
            root,
            base,
            ..
        } = self;

        let Some(Overlay { layout, .. }) = overlay.as_ref() else {
            return;
        };

        let overlay = root
            .as_widget_mut()
            .overlay(
                &mut self.state,
                Layout::new(base),
                renderer,
                &viewport,
                Vector::ZERO,
            )
            .map(overlay::Nested::new);

        if let Some(mut overlay) = overlay {
            overlay.draw(renderer, theme, style, Layout::new(layout), cursor);
        }
    }

    /// Applies a [`widget::Operation`] to the [`UserInterface`].
    pub fn operate(
        &mut self,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        let viewport = Rectangle::with_size(self.bounds);

        self.root.as_widget_mut().operate(
            &mut self.state,
            Layout::new(&self.base),
            renderer,
            operation,
        );

        if let Some(mut overlay) = self
            .root
            .as_widget_mut()
            .overlay(
                &mut self.state,
                Layout::new(&self.base),
                renderer,
                &viewport,
                Vector::ZERO,
            )
            .map(overlay::Nested::new)
        {
            if self.overlay.is_none() {
                self.overlay = Some(Overlay {
                    layout: overlay.layout(renderer, self.bounds),
                    interaction: mouse::Interaction::None,
                });
            }

            overlay.operate(
                Layout::new(&self.overlay.as_ref().unwrap().layout),
                renderer,
                operation,
            );
        }
    }

    /// Relayouts and returns a new  [`UserInterface`] using the provided
    /// bounds.
    pub fn relayout(self, bounds: Size, renderer: &mut Renderer) -> Self {
        Self::build(
            self.root,
            bounds,
            Cache {
                state: self.state,
                cursor: self.cursor,
            },
            renderer,
        )
    }

    /// Extract the [`Cache`] of the [`UserInterface`], consuming it in the
    /// process.
    pub fn into_cache(self) -> Cache {
        Cache {
            state: self.state,
            cursor: self.cursor,
        }
    }

    /// get a11y nodes
    #[cfg(feature = "a11y")]
    pub fn a11y_nodes(
        &self,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.root.as_widget().a11y_nodes(
            Layout::new(&self.base),
            &self.state,
            cursor,
        )
    }

    /// Find widget with given id
    pub fn find(&self, id: &widget::Id) -> Option<&widget::Tree> {
        self.state.find(id)
    }

    /// Get the destination rectangles for the user interface.
    pub fn dnd_rectangles(
        &self,
        prev_capacity: usize,
        renderer: &Renderer,
    ) -> DndDestinationRectangles {
        let mut ret = DndDestinationRectangles::with_capacity(prev_capacity);
        self.root.as_widget().drag_destinations(
            &self.state,
            Layout::new(&self.base),
            renderer,
            &mut ret,
        );
        ret
    }
}

/// Reusable data of a specific [`UserInterface`].
#[derive(Debug)]
pub struct Cache {
    state: widget::Tree,
    cursor: Option<mouse::Cursor>,
}

impl Cache {
    /// Creates an empty [`Cache`].
    ///
    /// You should use this to initialize a [`Cache`] before building your first
    /// [`UserInterface`].
    pub fn new() -> Cache {
        Cache {
            state: widget::Tree::empty(),
            cursor: None,
        }
    }
}

impl Default for Cache {
    fn default() -> Cache {
        Cache::new()
    }
}

/// The resulting state after updating a [`UserInterface`].
#[derive(Clone)]
pub enum State {
    /// The [`UserInterface`] is outdated and needs to be rebuilt.
    Outdated,

    /// The [`UserInterface`] is up-to-date and can be reused without
    /// rebuilding.
    Updated {
        /// The current [`mouse::Interaction`] of the user interface.
        mouse_interaction: mouse::Interaction,
        /// The [`window::RedrawRequest`] describing when a redraw should be performed.
        redraw_request: window::RedrawRequest,
        /// The current [`InputMethod`] strategy of the user interface.
        input_method: InputMethod,
        /// Whether the layout of the [`UserInterface`] has changed.
        has_layout_changed: bool,
    },
}

impl State {
    /// Returns whether the layout of the [`UserInterface`] has changed.
    pub fn has_layout_changed(&self) -> bool {
        match self {
            State::Outdated => true,
            State::Updated {
                has_layout_changed, ..
            } => *has_layout_changed,
        }
    }
}
