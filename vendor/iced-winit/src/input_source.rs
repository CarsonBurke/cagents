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
