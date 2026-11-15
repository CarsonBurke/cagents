use super::*;

fn counted_wire_firing() -> Firing {
    Firing {
        id: Uuid::new_v4(),
        automation: Uuid::new_v4(),
        name: "First snapshot".into(),
        revision: 7,
        trigger: Trigger::Scheduled {
            occurrence: Occurrence {
                due_ms: 6_000_000,
                local: "1970-01-01T01:40".into(),
            },
        },
        state: FiringState::Skipped,
        release_pending: false,
        cancel_requested: false,
        created_ms: 60_000,
        started_ms: None,
        finished_ms: Some(6_000_000),
        agent: None,
        run: None,
        skip: Some(SkipReason::Overlap),
        diagnostic: None,
        skipped_occurrences: Some(SkippedOccurrences {
            count: 100,
            first_occurrence_ms: 60_000,
            last_occurrence_ms: 6_000_000,
        }),
    }
}
#[test]
fn counted_skip_range_roundtrips_json_and_matched_shared_wire() {
    let firing = counted_wire_firing();
    assert_eq!(
        serde_json::from_value::<Firing>(serde_json::to_value(&firing).unwrap()).unwrap(),
        firing
    );
    assert_eq!(
        postcard::from_bytes::<Firing>(&postcard::to_allocvec(&firing).unwrap()).unwrap(),
        firing
    );
}
#[test]
fn published_legacy_firing_json_defaults_to_no_count_not_a_fabricated_interval() {
    let mut firing = counted_wire_firing();
    firing.skip = Some(SkipReason::MissedDeadline);
    firing.skipped_occurrences = None;
    let mut legacy = serde_json::to_value(&firing).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("skipped_occurrences");
    assert_eq!(serde_json::from_value::<Firing>(legacy).unwrap(), firing);
}
