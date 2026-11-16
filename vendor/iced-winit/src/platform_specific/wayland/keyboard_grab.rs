//! Client-side ownership of the keyboard by grabbing popups.
//!
//! A popup created with `xdg_popup.grab` owns its toplevel's keyboard input
//! until it closes. Compositors do not all move `wl_keyboard` focus to the
//! popup when the grab starts, though. Smithay-based compositors such as niri
//! leave focus on the toplevel and transfer it lazily, when the next key
//! passes through the seat's keyboard grab. Keys that bypass that grab, from a
//! virtual keyboard or while an input method holds its own keyboard grab, keep
//! arriving at the toplevel. Without this routing, a context menu would never
//! see Up, Down, Enter or Escape there, and the widget beneath it would take
//! them instead, for example editing the text field the menu belongs to.
//!
//! Toolkits keep their own grab state for this reason. Here, while a grabbing
//! popup is open, key presses and releases delivered to its toplevel go to the
//! topmost grabbing popup instead, just as they would once the compositor
//! moves focus. So does text an input method composes or commits, which would
//! otherwise still reach the toplevel's focused field. Modifier changes and
//! the input method opening or closing still reach the toplevel as well, so
//! its widgets do not keep a stale state after the popup closes.

use iced_runtime::core::{Event, input_method, keyboard, window};

/// Open grabbing popups, oldest first, with the toplevel each one belongs to.
#[derive(Debug, Default)]
pub(crate) struct KeyboardGrabs {
    grabs: Vec<Grab>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grab {
    toplevel: window::Id,
    popup: window::Id,
}

impl KeyboardGrabs {
    /// Records a grabbing popup created for `toplevel`.
    pub(crate) fn insert(&mut self, toplevel: window::Id, popup: window::Id) {
        self.remove(popup);
        self.grabs.push(Grab { toplevel, popup });
    }

    /// Forgets a popup once it is done, whether the compositor dismissed it
    /// or the application destroyed it.
    pub(crate) fn remove(&mut self, popup: window::Id) {
        self.grabs.retain(|grab| grab.popup != popup);
    }

    /// The popup that owns `toplevel`'s keyboard input, if any.
    fn owner(&self, toplevel: window::Id) -> Option<window::Id> {
        self.grabs
            .iter()
            .rev()
            .find(|grab| grab.toplevel == toplevel)
            .map(|grab| grab.popup)
    }

    /// Sends keyboard input addressed to a grabbed toplevel to the popup that
    /// owns it. `is_open` reports whether a popup surface still exists; a
    /// popup that has gone without a `Done` event is forgotten.
    pub(crate) fn route(
        &mut self,
        events: &mut Vec<(Option<window::Id>, Event)>,
        is_open: impl Fn(window::Id) -> bool,
    ) {
        self.grabs.retain(|grab| is_open(grab.popup));
        if self.grabs.is_empty() {
            return;
        }

        let mut routed = Vec::with_capacity(events.len());
        for (id, event) in events.drain(..) {
            let owner = id.and_then(|toplevel| self.owner(toplevel));
            match (owner, &event) {
                (
                    Some(popup),
                    Event::Keyboard(
                        keyboard::Event::KeyPressed { .. }
                        | keyboard::Event::KeyReleased { .. },
                    )
                    | Event::InputMethod(
                        input_method::Event::Preedit(..)
                        | input_method::Event::Commit(_),
                    ),
                ) => routed.push((Some(popup), event)),
                (
                    Some(popup),
                    Event::Keyboard(keyboard::Event::ModifiersChanged(_)),
                ) => {
                    routed.push((Some(popup), event.clone()));
                    routed.push((id, event));
                }
                _ => routed.push((id, event)),
            }
        }
        *events = routed;
    }
}

#[cfg(test)]
mod tests {
    use super::KeyboardGrabs;
    use iced_runtime::core::{
        Event, input_method, keyboard,
        keyboard::{Key, Location, Modifiers, key},
        mouse, window,
    };

    fn key(event: fn(Key) -> keyboard::Event, named: key::Named) -> Event {
        Event::Keyboard(event(Key::Named(named)))
    }

    fn pressed(key: Key) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            physical_key: key::Physical::Code(key::Code::ArrowDown),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
            key,
        }
    }

    fn released(key: Key) -> keyboard::Event {
        keyboard::Event::KeyReleased {
            modified_key: key.clone(),
            physical_key: key::Physical::Code(key::Code::ArrowDown),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            key,
        }
    }

    fn typed(text: &str) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character(text.into()),
            modified_key: Key::Character(text.into()),
            physical_key: key::Physical::Code(key::Code::KeyX),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: Some(text.into()),
            repeat: false,
        })
    }

    #[test]
    fn grabbing_popup_receives_its_toplevel_keys() {
        let main = window::Id::unique();
        let other = window::Id::unique();
        let menu = window::Id::unique();
        let mut grabs = KeyboardGrabs::default();
        grabs.insert(main, menu);

        let shift = Event::Keyboard(keyboard::Event::ModifiersChanged(
            Modifiers::SHIFT,
        ));
        let click = Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ));
        let composed = Event::InputMethod(input_method::Event::Preedit(
            "x".into(),
            None,
        ));
        let committed =
            Event::InputMethod(input_method::Event::Commit("x".into()));
        let ime_closed = Event::InputMethod(input_method::Event::Closed);
        let mut events = vec![
            (Some(main), key(pressed, key::Named::ArrowDown)),
            (Some(main), key(released, key::Named::ArrowDown)),
            (Some(main), key(pressed, key::Named::Enter)),
            (Some(main), key(pressed, key::Named::Escape)),
            (Some(main), typed("x")),
            (Some(main), shift.clone()),
            (Some(main), composed.clone()),
            (Some(main), committed.clone()),
            (Some(main), ime_closed.clone()),
            (Some(main), click.clone()),
            (Some(other), key(pressed, key::Named::Enter)),
            (Some(menu), key(pressed, key::Named::ArrowUp)),
            (None, key(pressed, key::Named::Enter)),
        ];
        grabs.route(&mut events, |_| true);

        assert_eq!(
            events,
            vec![
                (Some(menu), key(pressed, key::Named::ArrowDown)),
                (Some(menu), key(released, key::Named::ArrowDown)),
                (Some(menu), key(pressed, key::Named::Enter)),
                (Some(menu), key(pressed, key::Named::Escape)),
                (Some(menu), typed("x")),
                (Some(menu), shift.clone()),
                (Some(main), shift),
                (Some(menu), composed),
                (Some(menu), committed),
                (Some(main), ime_closed),
                (Some(main), click),
                (Some(other), key(pressed, key::Named::Enter)),
                (Some(menu), key(pressed, key::Named::ArrowUp)),
                (None, key(pressed, key::Named::Enter)),
            ],
        );
    }

    #[test]
    fn keys_return_to_the_toplevel_when_the_popup_closes() {
        let main = window::Id::unique();
        let menu = window::Id::unique();
        let submenu = window::Id::unique();
        let mut grabs = KeyboardGrabs::default();
        grabs.insert(main, menu);
        grabs.insert(main, submenu);

        let mut events = vec![(Some(main), key(pressed, key::Named::Enter))];
        grabs.route(&mut events, |_| true);
        assert_eq!(events[0].0, Some(submenu), "the topmost popup owns keys");

        grabs.remove(submenu);
        let mut events = vec![(Some(main), key(pressed, key::Named::Enter))];
        grabs.route(&mut events, |_| true);
        assert_eq!(events[0].0, Some(menu));

        // A popup whose surface is gone no longer holds the keyboard, even if
        // its `Done` event never arrived.
        let mut events = vec![(Some(main), key(pressed, key::Named::Enter))];
        grabs.route(&mut events, |popup| popup != menu);
        assert_eq!(events[0].0, Some(main));

        let mut events = vec![(Some(main), key(pressed, key::Named::Enter))];
        grabs.route(&mut events, |_| true);
        assert_eq!(events[0].0, Some(main), "the stale grab was forgotten");
    }
}
