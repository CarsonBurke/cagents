//! Durable terminal markers attach only to explicit, visible item bindings.
use crate::{agent::ActivityOutcome, chat};
use std::{collections::{HashMap, HashSet}, hash::{Hash, Hasher}};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Tone {
    Finished,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Marker {
    pub label: String,
    pub detail: String,
    pub tone: Tone,
    pub signature: u64,
}

impl Marker {
    fn from_record(record: &chat::turn_outcomes::TurnOutcome) -> Self {
        let outcome = &record.outcome;
        let tone = match outcome {
            ActivityOutcome::Succeeded => Tone::Finished,
            ActivityOutcome::Cancelled => Tone::Cancelled,
            ActivityOutcome::Failed { .. } | ActivityOutcome::Interrupted { .. } => Tone::Failed,
        };
        // Hydrated history records when the terminal evidence was first seen,
        // which may be much later than the provider's actual completion.
        let time = i64::try_from(record.finished_at / 1000)
            .ok()
            .and_then(chat::local_time);
        let label = time.as_ref().map_or_else(
            || outcome.label().to_owned(),
            |time| format!("{} · observed {}", outcome.label(), time.clock()),
        );
        let mut detail = time.map_or_else(
            || outcome.label().to_owned(),
            |time| format!("{} · observed {:04}-{:02}-{:02} {}", outcome.label(), time.year + 1900, time.month + 1, time.day, time.clock()),
        );
        if let Some(reason) = outcome.reason() {
            detail.push_str(" · ");
            detail.push_str(reason);
        }
        let mut hasher = std::hash::DefaultHasher::new();
        (&label, &detail, tone).hash(&mut hasher);
        Self { label, detail, tone, signature: hasher.finish() }
    }
}

/// Item identity and role decide marker placement. Content bodies and reason
/// strings do not participate in this once-per-snapshot window fingerprint.
pub(super) fn window_signature(snapshot: &chat::Snapshot) -> u64 {
    window_signature_with_alias(snapshot, |_| None)
}

pub(super) fn window_signature_with_alias<'a>(snapshot: &chat::Snapshot, logical_item_id: impl Fn(&str) -> Option<&'a str>) -> u64 {
    if snapshot.turn_outcomes.records.is_empty() { return 0; }
    let mut hasher = std::hash::DefaultHasher::new();
    snapshot.items.len().hash(&mut hasher);
    for item in &snapshot.items {
        (&item.id, item.role as u8, matches!(item.content, chat::ItemContent::Delivery { .. })).hash(&mut hasher);
    }
    // Logical-source aliases can change while the displayed root and the
    // outcome projection are unchanged. Their explicit proof is cache input.
    for record in &snapshot.turn_outcomes.records {
        for item in &record.item_ids { logical_item_id(item).hash(&mut hasher); }
    }
    hasher.finish()
}

/// One marker per proved result, beside its last visible user part when one
/// exists. Otherwise its last explicitly bound item carries it. This is only
/// placement: no presentation group or ID spelling grants turn ownership.
pub(super) fn project(snapshot: &chat::Snapshot) -> HashMap<String, Marker> {
    project_with_alias(snapshot, |_| None)
}

/// After logical-source projection, only its explicit reverse alias registry
/// may join a transport part to a currently displayed root. Unknown spellings
/// and aliases outside the visible window grant no fallback placement.
pub(super) fn project_with_alias<'a>(snapshot: &chat::Snapshot, logical_item_id: impl Fn(&str) -> Option<&'a str>) -> HashMap<String, Marker> {
    let projection = &snapshot.turn_outcomes;
    if projection.records.is_empty() || projection.validate().is_err() {
        return HashMap::new();
    }
    let Some(session) = snapshot.session_id.as_deref() else {
        return HashMap::new();
    };
    let visible = snapshot.items.iter().enumerate()
        .map(|(index, item)| (item.id.as_str(), (index, item.as_ref())))
        .collect::<HashMap<_, _>>();
    let mut markers = HashMap::new();
    let mut conflicts = HashSet::new();
    for record in &projection.records {
        if record.outcome.identity.session_id != session { continue; }
        let item = record.item_ids.iter().filter_map(|id| visible.get(id.as_str())
            .or_else(|| logical_item_id(id).and_then(|root| visible.get(root))))
            .max_by_key(|(index, item)| (
                item.role == chat::Role::User && !matches!(item.content, chat::ItemContent::Delivery { .. }),
                *index,
            ));
        if let Some((_, item)) = item {
            if conflicts.contains(&item.id) { continue; }
            if markers.remove(&item.id).is_some() {
                conflicts.insert(item.id.clone());
            } else {
                markers.insert(item.id.clone(), Marker::from_record(&record.outcome));
            }
        }
    }
    markers
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat::turn_outcomes::{TurnIdentity, TurnIdentityKind, TurnOutcome, TurnOutcomeProjection, VisibleTurnOutcome};
    use std::sync::Arc;

    fn item(id: &str, role: chat::Role) -> Arc<chat::Item> {
        Arc::new(chat::Item { id: id.into(), role, started_at: None, at: None,
            content: chat::ItemContent::Text { text: id.into(), thinking: false, phase: chat::Phase::Final } })
    }
    fn record(turn: &str, ids: &[&str], outcome: ActivityOutcome) -> VisibleTurnOutcome {
        VisibleTurnOutcome { outcome: TurnOutcome { identity: TurnIdentity { session_id: "native-session".into(), kind: TurnIdentityKind::AcceptedInput, id: turn.into() }, outcome, finished_at: 1234 }, item_ids: ids.iter().map(|id| (*id).into()).collect() }
    }

    #[test]
    fn attention_turn_marker_deduplicates_exact_parts_and_prefers_the_bound_user_bubble() {
        let snapshot = chat::Snapshot { session_id: Some("native-session".into()),
            items: vec![item("actual-part-one", chat::Role::User), item("unbound-neighbor", chat::Role::User), item("actual-part-two", chat::Role::User), item("owned-response", chat::Role::Assistant)],
            turn_outcomes: Arc::new(TurnOutcomeProjection { revision: 1, complete: true,
                records: vec![record("confirmed-input", &["actual-part-one", "owned-response", "actual-part-two"], ActivityOutcome::Cancelled)] }), ..Default::default() };
        let markers = project(&snapshot);
        assert_eq!(markers.len(), 1);
        assert!(markers["actual-part-two"].label.starts_with("Cancelled"));
        assert!(!markers.contains_key("unbound-neighbor"));
        let mut step_only = snapshot.clone();
        step_only.items = vec![item("owned-response", chat::Role::Assistant)];
        assert_eq!(project(&step_only).keys().map(String::as_str).collect::<Vec<_>>(), vec!["owned-response"]);
        step_only.items = vec![item("unbound-neighbor", chat::Role::User)];
        assert!(project(&step_only).is_empty(), "no nearest-user or group-key fallback is allowed");
        step_only.items = snapshot.items;
        step_only.session_id = Some("another-native-session".into());
        assert!(project(&step_only).is_empty(), "saved results cannot cross native conversation identity");
    }

    #[test]
    fn attention_turn_markers_follow_only_explicit_visible_logical_root_aliases() {
        let mut snapshot = chat::Snapshot { session_id: Some("native-session".into()),
            items: vec![item("opaque/root", chat::Role::User)],
            turn_outcomes: Arc::new(TurnOutcomeProjection { revision: 1, complete: true,
                records: vec![record("confirmed-input", &["later-transport-part"], ActivityOutcome::Cancelled)] }), ..Default::default() };
        let alias = |raw: &str| (raw == "later-transport-part").then_some("opaque/root");
        assert!(project(&snapshot).is_empty());
        assert!(project_with_alias(&snapshot, alias).contains_key("opaque/root"));
        assert_ne!(window_signature(&snapshot), window_signature_with_alias(&snapshot, alias),
            "new alias proof invalidates placement even when root IDs/projection stay the same");
        let unknown = |_: &str| None::<&str>;
        assert!(project_with_alias(&snapshot, unknown).is_empty());
        snapshot.items = vec![item("elsewhere/root", chat::Role::User)];
        assert!(project_with_alias(&snapshot, alias).is_empty(), "proved aliases cannot target an off-page root");
        snapshot.items = vec![item("opaque/root", chat::Role::User)];
        Arc::make_mut(&mut snapshot.turn_outcomes).records.push(record("different-turn", &["other-part"], ActivityOutcome::Succeeded));
        let ambiguous = |raw: &str| matches!(raw, "later-transport-part" | "other-part").then_some("opaque/root");
        assert!(project_with_alias(&snapshot, ambiguous).is_empty(), "two terminal identities cannot claim one displayed root");
    }

    #[test]
    fn attention_turn_marker_labels_keep_each_terminal_outcome_and_failure_reason() {
        for (outcome, label, tone) in [
            (ActivityOutcome::Succeeded, "Finished", Tone::Finished),
            (ActivityOutcome::Cancelled, "Cancelled", Tone::Cancelled),
            (ActivityOutcome::Failed { reason: "exit code 3".into() }, "Failed", Tone::Failed),
            (ActivityOutcome::Interrupted { reason: "missing final result".into() }, "Interrupted", Tone::Failed),
        ] {
            let expected_reason = outcome.reason().map(str::to_owned);
            let record = record("input", &["item"], outcome);
            let marker = Marker::from_record(&record.outcome);
            assert!(marker.label.starts_with(label));
            assert!(marker.label.contains("observed"));
            assert!(marker.detail.contains("observed"));
            assert_eq!(marker.tone, tone);
            if let Some(reason) = expected_reason { assert!(marker.detail.contains(&reason)); }
        }
    }
}
