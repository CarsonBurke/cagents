//! One logical global-physical stream per run_instance, shared by all producers.
use crate::core::{
    Event,
    keyboard::{
        self,
        input::{PressCycle, Source, Transition},
    },
    window,
};
use std::{collections::HashMap, ops::RangeFull, sync::Arc};

type Buffered = (Option<window::Id>, Event, Source);

#[derive(Default)]
pub(crate) struct NativeEvents {
    pending: Vec<Buffered>,
    down: HashMap<keyboard::key::Physical, Arc<PressCycle>>,
}
impl NativeEvents {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub(crate) fn push(&mut self, (window, event): (Option<window::Id>, Event)) {
        let source = match &event {
            Event::Keyboard(keyboard::Event::KeyPressed { physical_key, .. }) => {
                // repeat:false is not proof of a new cycle after refocus.
                let cycle = self
                    .down
                    .entry(*physical_key)
                    .or_insert_with(|| PressCycle::new(*physical_key))
                    .clone();
                Source::tracked(cycle, Transition::Press)
            }
            Event::Keyboard(keyboard::Event::KeyReleased { physical_key, .. }) => {
                if let Some(cycle) = self.down.remove(physical_key) {
                    cycle.release();
                    Source::tracked(cycle, Transition::Release)
                } else {
                    Source::default()
                }
            }
            _ => Source::default(),
        };
        self.pending.push((window, event, source));
    }
    pub(crate) fn take_window(&mut self, id: window::Id) -> (Vec<Event>, Vec<Source>) {
        let mut events = Vec::new();
        let mut sources = Vec::new();
        self.pending.retain(|(window, event, source)| {
            if *window == Some(id) {
                events.push(event.clone());
                sources.push(source.clone());
                false
            } else {
                true
            }
        });
        (events, sources)
    }
    pub(crate) fn drain(&mut self, range: RangeFull) -> std::vec::Drain<'_, Buffered> {
        self.pending.drain(range)
    }
}
impl Drop for NativeEvents {
    fn drop(&mut self) {
        for cycle in self.down.values() {
            cycle.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::{Key, Location, Modifiers, key};

    const PHYSICAL: key::Physical = key::Physical::Code(key::Code::Enter);

    fn press(repeat: bool) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(key::Named::Enter),
            modified_key: Key::Named(key::Named::Enter),
            physical_key: PHYSICAL,
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat,
        })
    }

    fn release() -> Event {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(key::Named::Enter),
            modified_key: Key::Named(key::Named::Enter),
            physical_key: PHYSICAL,
            location: Location::Standard,
            modifiers: Modifiers::empty(),
        })
    }

    #[test]
    fn queued_old_release_keeps_old_cycle_after_new_press() {
        let id = window::Id::unique();
        let mut queue = NativeEvents::default();
        queue.push((Some(id), press(false)));
        queue.push((Some(id), release()));
        queue.push((Some(id), press(false)));

        let (events, sources) = queue.take_window(id);
        assert_eq!(events, [press(false), release(), press(false)]);
        let first = sources[0].cycle().unwrap();
        let old_release = sources[1].cycle().unwrap();
        let second = sources[2].cycle().unwrap();
        assert!(Arc::ptr_eq(first, old_release));
        assert!(!Arc::ptr_eq(first, second));
        assert!(!first.is_down());
        assert!(second.is_down());
        assert_eq!(sources[1].transition(), Some(Transition::Release));
        assert_eq!(sources[2].transition(), Some(Transition::Press));
        // A delayed subscriber owns this same old token, even after draining.
        let delayed_release = sources[1].clone();
        assert!(queue.drain(..).next().is_none());
        assert!(!Arc::ptr_eq(delayed_release.cycle().unwrap(), second));
    }

    #[test]
    fn held_cycle_spans_surfaces_at_the_shared_producer_boundary() {
        let main = window::Id::unique();
        let popup = window::Id::unique();
        let mut queue = NativeEvents::default();
        // Winit and custom SCTK producers both call this same queue.push API.
        queue.push((Some(popup), press(false)));
        queue.push((Some(main), press(true)));
        queue.push((Some(main), release()));

        // UI window order may differ from native enqueue order.
        let (_, main_sources) = queue.take_window(main);
        let (_, popup_sources) = queue.take_window(popup);
        let original = popup_sources[0].cycle().unwrap();
        assert!(Arc::ptr_eq(original, main_sources[0].cycle().unwrap()));
        assert!(Arc::ptr_eq(original, main_sources[1].cycle().unwrap()));
        assert!(!original.is_down());

        #[cfg(wayland_platform)]
        {
            use crate::platform_specific::wayland::sctk_event::KeyboardEventVariant;
            use cctk::sctk::seat::keyboard::{KeyEvent, Modifiers as SctkModifiers};
            use winit::platform::scancode::PhysicalKeyExtScancode;
            use xkeysym::{Keysym, key as sym};

            let modifiers = SctkModifiers {
                ctrl: true,
                shift: true,
                ..SctkModifiers::default()
            };
            for (raw, keysym, expected_physical, key, location, text) in [
                (
                    1,
                    sym::Escape,
                    key::Physical::Code(key::Code::Escape),
                    Key::Named(key::Named::Escape),
                    Location::Standard,
                    None,
                ),
                (
                    15,
                    sym::Tab,
                    key::Physical::Code(key::Code::Tab),
                    Key::Named(key::Named::Tab),
                    Location::Standard,
                    Some("\t"),
                ),
                (
                    28,
                    sym::Return,
                    key::Physical::Code(key::Code::Enter),
                    Key::Named(key::Named::Enter),
                    Location::Standard,
                    Some("\r"),
                ),
                (
                    57,
                    sym::space,
                    key::Physical::Code(key::Code::Space),
                    Key::Character(" ".into()),
                    Location::Standard,
                    Some(" "),
                ),
                (
                    96,
                    sym::KP_Enter,
                    key::Physical::Code(key::Code::NumpadEnter),
                    Key::Named(key::Named::Enter),
                    Location::Numpad,
                    Some("\r"),
                ),
                (
                    103,
                    sym::Up,
                    key::Physical::Code(key::Code::ArrowUp),
                    Key::Named(key::Named::ArrowUp),
                    Location::Standard,
                    None,
                ),
                (
                    105,
                    sym::Left,
                    key::Physical::Code(key::Code::ArrowLeft),
                    Key::Named(key::Named::ArrowLeft),
                    Location::Standard,
                    None,
                ),
                (
                    106,
                    sym::Right,
                    key::Physical::Code(key::Code::ArrowRight),
                    Key::Named(key::Named::ArrowRight),
                    Location::Standard,
                    None,
                ),
                (
                    108,
                    sym::Down,
                    key::Physical::Code(key::Code::ArrowDown),
                    Key::Named(key::Named::ArrowDown),
                    Location::Standard,
                    None,
                ),
                (
                    30,
                    sym::eacute,
                    key::Physical::Code(key::Code::KeyA),
                    Key::Character("é".into()),
                    Location::Standard,
                    Some("é"),
                ),
                (
                    20,
                    sym::t,
                    key::Physical::Code(key::Code::KeyT),
                    Key::Character("t".into()),
                    Location::Standard,
                    Some("t"),
                ),
                (
                    46,
                    sym::c,
                    key::Physical::Code(key::Code::KeyC),
                    Key::Character("c".into()),
                    Location::Standard,
                    Some("c"),
                ),
                (
                    133,
                    sym::XF86_Copy,
                    key::Physical::Code(key::Code::Copy),
                    Key::Named(key::Named::Copy),
                    Location::Standard,
                    None,
                ),
                (
                    137,
                    sym::XF86_Cut,
                    key::Physical::Code(key::Code::Cut),
                    Key::Named(key::Named::Cut),
                    Location::Standard,
                    None,
                ),
                (
                    131,
                    sym::Undo,
                    key::Physical::Code(key::Code::Undo),
                    Key::Named(key::Named::Undo),
                    Location::Standard,
                    None,
                ),
                (
                    158,
                    sym::XF86_Back,
                    key::Physical::Code(key::Code::BrowserBack),
                    Key::Named(key::Named::BrowserBack),
                    Location::Standard,
                    None,
                ),
                (
                    0,
                    sym::Escape,
                    key::Physical::Unidentified(key::NativeCode::Xkb(0)),
                    Key::Named(key::Named::Escape),
                    Location::Standard,
                    None,
                ),
                (
                    u32::MAX,
                    sym::Escape,
                    key::Physical::Unidentified(key::NativeCode::Xkb(u32::MAX)),
                    Key::Named(key::Named::Escape),
                    Location::Standard,
                    None,
                ),
            ] {
                let native = KeyEvent {
                    time: 7,
                    raw_code: raw,
                    keysym: Keysym::new(keysym),
                    utf8: text.map(str::to_owned),
                };
                // This is winit's selected Linux scancode mapper, not a copy
                // of the SCTK mapping table or a test-only offset adapter.
                let physical = crate::conversion::physical_key(
                    winit::keyboard::PhysicalKey::from_scancode(raw),
                );
                assert_eq!(physical, expected_physical);
                let expected_press = keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key.clone(),
                    physical_key: physical,
                    location,
                    modifiers: Modifiers::CTRL | Modifiers::SHIFT,
                    text: text.map(Into::into),
                    repeat: false,
                };
                let mut expected_repeat = expected_press.clone();
                let keyboard::Event::KeyPressed { repeat, .. } = &mut expected_repeat else {
                    unreachable!();
                };
                *repeat = true;
                let expected_release = keyboard::Event::KeyReleased {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: physical,
                    location,
                    modifiers: Modifiers::CTRL | Modifiers::SHIFT,
                };
                // The same conversion method is called by SctkEvent::process
                // for the three real producer variants.
                let sctk_press = KeyboardEventVariant::Press(native.clone())
                    .into_key_event(modifiers)
                    .unwrap();
                let sctk_repeat = KeyboardEventVariant::Repeat(native.clone())
                    .into_key_event(modifiers)
                    .unwrap();
                let sctk_release = KeyboardEventVariant::Release(native)
                    .into_key_event(modifiers)
                    .unwrap();
                assert_eq!(sctk_press, expected_press);
                assert_eq!(sctk_repeat, expected_repeat);
                assert_eq!(sctk_release, expected_release);

                let mut queue = NativeEvents::default();
                queue.push((Some(popup), Event::Keyboard(sctk_press)));
                queue.push((Some(main), Event::Keyboard(expected_repeat)));
                queue.push((Some(popup), Event::Keyboard(sctk_repeat)));
                queue.push((Some(popup), Event::Keyboard(sctk_release)));
                queue.push((Some(main), Event::Keyboard(expected_press)));
                queue.push((Some(main), Event::Keyboard(expected_release)));
                // Drain main first to retain the real cross-window delivery
                // permutation: its held repeat must still use popup's C1.
                let (_, main_sources) = queue.take_window(main);
                let (_, popup_sources) = queue.take_window(popup);
                let first = popup_sources[0].cycle().unwrap();
                assert!(Arc::ptr_eq(first, popup_sources[1].cycle().unwrap()));
                assert!(Arc::ptr_eq(first, popup_sources[2].cycle().unwrap()));
                assert!(Arc::ptr_eq(first, main_sources[0].cycle().unwrap()));
                let fresh = main_sources[1].cycle().unwrap();
                assert!(!Arc::ptr_eq(first, fresh));
                assert!(Arc::ptr_eq(fresh, main_sources[2].cycle().unwrap()));
                assert!(!first.is_down() && !fresh.is_down());
                assert!(queue.is_empty());
            }
        }
    }

    #[test]
    fn repeat_false_while_down_is_not_a_new_press_cycle() {
        let id = window::Id::unique();
        let mut queue = NativeEvents::default();
        queue.push((Some(id), press(false)));
        queue.push((Some(id), press(false)));
        queue.push((Some(id), press(true)));
        let (_, sources) = queue.take_window(id);
        assert_eq!(sources[0], sources[1]);
        assert_eq!(sources[0], sources[2]);
        assert!(sources[0].cycle().unwrap().is_down());
    }

    #[test]
    fn unmatched_release_has_no_cycle_or_transition() {
        let mut queue = NativeEvents::default();
        queue.push((None, release()));
        let (_, event, source) = queue.drain(..).next().unwrap();
        assert_eq!(event, release());
        assert_eq!(source, Source::default());
        assert!(source.cycle().is_none());
        assert!(source.transition().is_none());
    }

    #[test]
    fn window_selection_and_residual_drain_keep_event_source_alignment() {
        let selected = window::Id::unique();
        let other = window::Id::unique();
        let modifiers = Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::empty()));
        let mut queue = NativeEvents::default();
        queue.push((Some(other), press(false)));
        queue.push((Some(selected), modifiers.clone()));
        queue.push((None, release()));
        queue.push((Some(selected), press(false)));
        let (events, sources) = queue.take_window(selected);
        assert_eq!(events, [modifiers, press(false)]);
        assert_eq!(sources[0], Source::default());
        let new_cycle = sources[1].cycle().unwrap();

        let remaining: Vec<_> = queue.drain(..).collect();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].0, Some(other));
        assert_eq!(remaining[0].1, press(false));
        assert_eq!(remaining[1].0, None);
        assert_eq!(remaining[1].1, release());
        assert!(Arc::ptr_eq(
            remaining[0].2.cycle().unwrap(),
            remaining[1].2.cycle().unwrap(),
        ));
        assert!(!Arc::ptr_eq(remaining[1].2.cycle().unwrap(), new_cycle));
        assert!(queue.is_empty());
    }

    #[test]
    fn dropping_runtime_releases_cycle_without_retaining_it() {
        let id = window::Id::unique();
        let mut queue = NativeEvents::default();
        queue.push((Some(id), press(false)));
        let (_, sources) = queue.take_window(id);
        let cycle = sources[0].cycle().unwrap().clone();
        let weak = Arc::downgrade(&cycle);
        assert!(cycle.is_down());
        drop(queue);
        assert!(!cycle.is_down());
        drop(cycle);
        drop(sources);
        assert!(weak.upgrade().is_none());
    }
}
