//! Real snapshot projection for view regressions without the host implementation.
use crate::chat::{Event, Snapshot, now_ms as observed_at};

#[path = "runtime/child_diagnostics/projection.rs"]
mod child_diagnostics;
#[allow(dead_code)]
#[path = "runtime/child_text_copy.rs"]
mod child_text_copy;
#[path = "runtime/projection.rs"]
mod projection;

/// Project protocol events through the same reducer as the resident actor.
pub fn project_events_for_test(state: &mut Snapshot, events: Vec<Event>) {
    let mut indices = state
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id.clone(), index))
        .collect();
    for event in events {
        projection::apply_projected_event(state, &mut indices, event, false);
    }
}
