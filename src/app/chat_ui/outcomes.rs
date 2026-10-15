//! Durable terminal markers attach only to explicit, visible item bindings.
use crate::{agent::ActivityOutcome, chat};
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Tone {
    Finished,
    Cancelled,
    Failed,
    ObservationEnded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Marker {
    pub label: String,
    pub detail: String,
    pub tone: Tone,
    pub signature: u64,
}

impl Marker {
    fn from_loss(record: &chat::turn_outcomes::VisibleInputObservationLost) -> Self {
        use chat::turn_outcomes::{ObservationLossCause as Cause, ObservationProcessRole as Role};
        let time = i64::try_from(record.loss.observed_at / 1000)
            .ok()
            .and_then(chat::local_time);
        let label = time.as_ref().map_or_else(
            || "Observation ended".to_owned(),
            |time| format!("Observation ended · observed {}", time.clock()),
        );
        // A correlated result frame can still have an unsupported cause. Only
        // native terminal proof establishes fate; do not claim no frame arrived.
        let mut detail =
            "Observation ended while this accepted input’s terminal outcome remained unproved."
                .to_owned();
        if let Some(time) = time {
            detail.push_str(&format!(
                " Observed {:04}-{:02}-{:02} {}.",
                time.year + 1900,
                time.month + 1,
                time.day,
                time.clock()
            ));
        }
        let cause = match &record.loss.cause {
            Cause::KernelProcessExit { role, code, signal } => {
                let process = match role {
                    Role::OwnedProtocolLeader => "Protocol process",
                    Role::ProvedRetainedProvider => "Observed provider process",
                };
                match (code, signal) {
                    (Some(code), None) => format!("{process} exited with code {code}."),
                    (None, Some(signal)) => format!("{process} exited after signal {signal}."),
                    _ => "Process observation ended.".to_owned(),
                }
            }
            Cause::ObserverDisconnected => "The connection ended.".to_owned(),
            Cause::ObserverFailed { reason } => reason.clone(),
            Cause::ObserverClosed => "Observation was closed.".to_owned(),
            Cause::ObservationSessionChanged => {
                "Observation moved to another conversation.".to_owned()
            }
        };
        detail.push(' ');
        detail.push_str(&cause);
        let tone = Tone::ObservationEnded;
        let mut hasher = std::hash::DefaultHasher::new();
        (&label, &detail, tone).hash(&mut hasher);
        Self {
            label,
            detail,
            tone,
            signature: hasher.finish(),
        }
    }

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
            |time| {
                format!(
                    "{} · observed {:04}-{:02}-{:02} {}",
                    outcome.label(),
                    time.year + 1900,
                    time.month + 1,
                    time.day,
                    time.clock()
                )
            },
        );
        if let Some(reason) = outcome.reason() {
            detail.push_str(" · ");
            detail.push_str(reason);
        }
        let mut hasher = std::hash::DefaultHasher::new();
        (&label, &detail, tone).hash(&mut hasher);
        Self {
            label,
            detail,
            tone,
            signature: hasher.finish(),
        }
    }
}

/// Item identity and role decide marker placement. Content bodies and reason
/// strings do not participate in this once-per-snapshot window fingerprint.
pub(super) fn window_signature(snapshot: &chat::Snapshot) -> u64 {
    window_signature_with_alias(snapshot, |_| None)
}

pub(super) fn window_signature_with_alias<'a>(
    snapshot: &chat::Snapshot,
    logical_item_id: impl Fn(&str) -> Option<&'a str>,
) -> u64 {
    if snapshot.turn_outcomes.records.is_empty() && snapshot.turn_outcomes.losses.is_empty() {
        return 0;
    }
    let mut hasher = std::hash::DefaultHasher::new();
    snapshot.items.len().hash(&mut hasher);
    for item in &snapshot.items {
        (
            &item.id,
            item.role as u8,
            matches!(item.content, chat::ItemContent::Delivery { .. }),
        )
            .hash(&mut hasher);
    }
    // Logical-source aliases can change while the displayed root and the
    // outcome projection are unchanged. Their explicit proof is cache input.
    for item in snapshot
        .turn_outcomes
        .records
        .iter()
        .flat_map(|record| &record.item_ids)
        .chain(
            snapshot
                .turn_outcomes
                .losses
                .iter()
                .flat_map(|record| &record.item_ids),
        )
    {
        logical_item_id(item).hash(&mut hasher);
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
pub(super) fn project_with_alias<'a>(
    snapshot: &chat::Snapshot,
    logical_item_id: impl Fn(&str) -> Option<&'a str>,
) -> HashMap<String, Marker> {
    let projection = &snapshot.turn_outcomes;
    if (projection.records.is_empty() && projection.losses.is_empty())
        || projection.validate().is_err()
    {
        return HashMap::new();
    }
    let Some(session) = snapshot.session_id.as_deref() else {
        return HashMap::new();
    };
    let visible = snapshot
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id.as_str(), (index, item.as_ref())))
        .collect::<HashMap<_, _>>();
    let mut markers = HashMap::new();
    let mut conflicts = HashSet::new();
    for record in &projection.records {
        if record.outcome.identity.session_id != session {
            continue;
        }
        let item = record
            .item_ids
            .iter()
            .filter_map(|id| {
                visible
                    .get(id.as_str())
                    .or_else(|| logical_item_id(id).and_then(|root| visible.get(root)))
            })
            .max_by_key(|(index, item)| {
                (
                    item.role == chat::Role::User
                        && !matches!(item.content, chat::ItemContent::Delivery { .. }),
                    *index,
                )
            });
        if let Some((_, item)) = item {
            place_marker(
                &mut markers,
                &mut conflicts,
                item,
                Marker::from_record(&record.outcome),
            );
        }
    }
    let terminal_identities: HashSet<_> = projection
        .records
        .iter()
        .map(|record| &record.outcome.identity)
        .collect();
    for loss in &projection.losses {
        if loss.accepted.input.session_id != session
            || terminal_identities.contains(&&loss.accepted.input)
            || loss
                .accepted
                .native_turn
                .as_ref()
                .is_some_and(|turn| terminal_identities.contains(&turn))
        {
            continue;
        }
        // An observation alias is only user-input proof. It never grants a
        // tool/assistant/neighbor anchor, including after logical projection.
        let item = loss
            .item_ids
            .iter()
            .filter_map(|id| {
                visible
                    .get(id.as_str())
                    .or_else(|| logical_item_id(id).and_then(|root| visible.get(root)))
            })
            .filter(|(_, item)| {
                item.role == chat::Role::User
                    && !matches!(item.content, chat::ItemContent::Delivery { .. })
            })
            .max_by_key(|(index, _)| *index);
        if let Some((_, item)) = item {
            place_marker(&mut markers, &mut conflicts, item, Marker::from_loss(loss));
        }
    }
    markers
}

fn place_marker(
    markers: &mut HashMap<String, Marker>,
    conflicts: &mut HashSet<String>,
    item: &chat::Item,
    marker: Marker,
) {
    if conflicts.contains(&item.id) {
        return;
    }
    if markers.remove(&item.id).is_some() {
        conflicts.insert(item.id.clone());
    } else {
        markers.insert(item.id.clone(), marker);
    }
}

#[cfg(test)]
pub(super) fn loss_for_test(
    session: &str,
    ids: &[&str],
) -> chat::turn_outcomes::VisibleInputObservationLost {
    use chat::turn_outcomes::*;
    let observer = InputObserver {
        id: uuid::Uuid::new_v4(),
        actor: uuid::Uuid::new_v4(),
        provider: crate::agent::Provider::Claude,
        native_session: session.into(),
        account_key: [1; 32],
        backend: ObservationBackend::OwnedProtocolProcess {
            instance: uuid::Uuid::new_v4(),
            pid: 100,
            start_ticks: Some(1),
        },
    };
    VisibleInputObservationLost {
        accepted: AcceptedInputObservation {
            input: TurnIdentity {
                session_id: session.into(),
                kind: TurnIdentityKind::AcceptedInput,
                id: "accepted-A".into(),
            },
            native_turn: None,
            observer: observer.clone(),
            source_incarnation: uuid::Uuid::new_v4(),
            observed_at: 1000,
        },
        loss: InputObservationLost {
            observer,
            event_id: uuid::Uuid::new_v4(),
            observed_at: 2000,
            cause: ObservationLossCause::ObserverDisconnected,
        },
        item_ids: ids.iter().map(|id| (*id).into()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat::turn_outcomes::{
        TurnIdentity, TurnIdentityKind, TurnOutcome, TurnOutcomeProjection, VisibleTurnOutcome,
    };
    use std::sync::Arc;

    fn item(id: &str, role: chat::Role) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role,
            started_at: None,
            at: None,
            content: chat::ItemContent::Text {
                text: id.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })
    }
    fn record(turn: &str, ids: &[&str], outcome: ActivityOutcome) -> VisibleTurnOutcome {
        VisibleTurnOutcome {
            outcome: TurnOutcome {
                identity: TurnIdentity {
                    session_id: "native-session".into(),
                    kind: TurnIdentityKind::AcceptedInput,
                    id: turn.into(),
                },
                outcome,
                finished_at: 1234,
            },
            item_ids: ids.iter().map(|id| (*id).into()).collect(),
        }
    }

    #[test]
    fn attention_turn_marker_deduplicates_exact_parts_and_prefers_the_bound_user_bubble() {
        let snapshot = chat::Snapshot {
            session_id: Some("native-session".into()),
            items: vec![
                item("actual-part-one", chat::Role::User),
                item("unbound-neighbor", chat::Role::User),
                item("actual-part-two", chat::Role::User),
                item("owned-response", chat::Role::Assistant),
            ],
            turn_outcomes: Arc::new(TurnOutcomeProjection {
                losses: Vec::new(),
                revision: 1,
                complete: true,
                records: vec![record(
                    "confirmed-input",
                    &["actual-part-one", "owned-response", "actual-part-two"],
                    ActivityOutcome::Cancelled,
                )],
            }),
            ..Default::default()
        };
        let markers = project(&snapshot);
        assert_eq!(markers.len(), 1);
        assert!(markers["actual-part-two"].label.starts_with("Cancelled"));
        assert!(!markers.contains_key("unbound-neighbor"));
        let mut step_only = snapshot.clone();
        step_only.items = vec![item("owned-response", chat::Role::Assistant)];
        assert_eq!(
            project(&step_only)
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["owned-response"]
        );
        step_only.items = vec![item("unbound-neighbor", chat::Role::User)];
        assert!(
            project(&step_only).is_empty(),
            "no nearest-user or group-key fallback is allowed"
        );
        step_only.items = snapshot.items;
        step_only.session_id = Some("another-native-session".into());
        assert!(
            project(&step_only).is_empty(),
            "saved results cannot cross native conversation identity"
        );
    }

    #[test]
    fn attention_turn_markers_follow_only_explicit_visible_logical_root_aliases() {
        let mut snapshot = chat::Snapshot {
            session_id: Some("native-session".into()),
            items: vec![item("opaque/root", chat::Role::User)],
            turn_outcomes: Arc::new(TurnOutcomeProjection {
                losses: Vec::new(),
                revision: 1,
                complete: true,
                records: vec![record(
                    "confirmed-input",
                    &["later-transport-part"],
                    ActivityOutcome::Cancelled,
                )],
            }),
            ..Default::default()
        };
        let alias = |raw: &str| (raw == "later-transport-part").then_some("opaque/root");
        assert!(project(&snapshot).is_empty());
        assert!(project_with_alias(&snapshot, alias).contains_key("opaque/root"));
        assert_ne!(
            window_signature(&snapshot),
            window_signature_with_alias(&snapshot, alias),
            "new alias proof invalidates placement even when root IDs/projection stay the same"
        );
        let unknown = |_: &str| None::<&str>;
        assert!(project_with_alias(&snapshot, unknown).is_empty());
        snapshot.items = vec![item("elsewhere/root", chat::Role::User)];
        assert!(
            project_with_alias(&snapshot, alias).is_empty(),
            "proved aliases cannot target an off-page root"
        );
        snapshot.items = vec![item("opaque/root", chat::Role::User)];
        Arc::make_mut(&mut snapshot.turn_outcomes)
            .records
            .push(record(
                "different-turn",
                &["other-part"],
                ActivityOutcome::Succeeded,
            ));
        let ambiguous = |raw: &str| {
            matches!(raw, "later-transport-part" | "other-part").then_some("opaque/root")
        };
        assert!(
            project_with_alias(&snapshot, ambiguous).is_empty(),
            "two terminal identities cannot claim one displayed root"
        );
    }

    #[test]
    fn attention_turn_marker_labels_keep_each_terminal_outcome_and_failure_reason() {
        for (outcome, label, tone) in [
            (ActivityOutcome::Succeeded, "Finished", Tone::Finished),
            (ActivityOutcome::Cancelled, "Cancelled", Tone::Cancelled),
            (
                ActivityOutcome::Failed {
                    reason: "exit code 3".into(),
                },
                "Failed",
                Tone::Failed,
            ),
            (
                ActivityOutcome::Interrupted {
                    reason: "missing final result".into(),
                },
                "Interrupted",
                Tone::Failed,
            ),
        ] {
            let expected_reason = outcome.reason().map(str::to_owned);
            let record = record("input", &["item"], outcome);
            let marker = Marker::from_record(&record.outcome);
            assert!(marker.label.starts_with(label));
            assert!(marker.label.contains("observed"));
            assert!(marker.detail.contains("observed"));
            assert_eq!(marker.tone, tone);
            if let Some(reason) = expected_reason {
                assert!(marker.detail.contains(&reason));
            }
        }
    }

    #[test]
    fn attention_observation_loss_labels_never_invent_a_terminal_input_outcome() {
        use chat::turn_outcomes::{
            ObservationBackend, ObservationLossCause as Cause, ObservationProcessRole as Role,
        };
        for cause in [
            Cause::ObserverDisconnected,
            Cause::ObserverClosed,
            Cause::ObservationSessionChanged,
            Cause::ObserverFailed {
                reason: "Connection could not be read".into(),
            },
            Cause::KernelProcessExit {
                role: Role::OwnedProtocolLeader,
                code: Some(3),
                signal: None,
            },
            Cause::KernelProcessExit {
                role: Role::ProvedRetainedProvider,
                code: None,
                signal: Some(15),
            },
        ] {
            let mut loss = loss_for_test("native-session", &["input"]);
            if matches!(
                &cause,
                Cause::KernelProcessExit {
                    role: Role::ProvedRetainedProvider,
                    ..
                }
            ) {
                loss.accepted.observer.backend = ObservationBackend::RetainedProvider {
                    instance: uuid::Uuid::new_v4(),
                    pid: 200,
                    start_ticks: 1,
                };
                loss.loss.observer = loss.accepted.observer.clone();
            }
            loss.loss.cause = cause;
            loss.validate().unwrap();
            let marker = Marker::from_loss(&loss);
            assert_eq!(marker.tone, Tone::ObservationEnded);
            assert!(marker.label.starts_with("Observation ended"));
            assert!(marker.detail.contains("terminal outcome remained unproved"));
            assert!(
                !marker.label.contains("Failed")
                    && !marker.label.contains("Cancelled")
                    && !marker.label.contains("Finished")
            );
            assert_eq!(
                marker,
                Marker::from_loss(&loss),
                "publication/paint time does not move the observed marker"
            );
        }
    }

    #[test]
    fn attention_observation_loss_requires_exact_visible_user_alias_and_current_native_session() {
        let mut snapshot = chat::Snapshot {
            session_id: Some("native-session".into()),
            busy: true,
            turn_generation: 2,
            items: vec![
                item("input-A", chat::Role::User),
                item("unknown-neighbor", chat::Role::User),
            ],
            turn_outcomes: Arc::new(TurnOutcomeProjection {
                revision: 1,
                complete: true,
                records: vec![],
                losses: vec![loss_for_test("native-session", &["input-A"])],
            }),
            ..Default::default()
        };
        let before = postcard::to_stdvec(&snapshot).unwrap();
        let markers = project(&snapshot);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers["input-A"].tone, Tone::ObservationEnded);
        assert!(!markers.contains_key("unknown-neighbor"));
        assert_eq!(
            postcard::to_stdvec(&snapshot).unwrap(),
            before,
            "presentation cannot mutate current B or native item status"
        );
        snapshot.items[0] = item("input-A", chat::Role::Assistant);
        assert!(
            project(&snapshot).is_empty(),
            "user proof does not grant an assistant/tool anchor"
        );
        snapshot.items[0] = item("input-A", chat::Role::User);
        snapshot.session_id = Some("another-native-session".into());
        assert!(project(&snapshot).is_empty());
        snapshot.session_id = Some("native-session".into());
        Arc::make_mut(&mut snapshot.turn_outcomes).losses[0]
            .accepted
            .observer
            .id = uuid::Uuid::nil();
        assert!(
            project(&snapshot).is_empty(),
            "malformed or conflicting observer identity is not display proof"
        );
    }

    #[test]
    fn attention_observation_loss_uses_only_proved_logical_user_roots_and_refuses_conflicts() {
        let mut snapshot = chat::Snapshot {
            session_id: Some("native-session".into()),
            items: vec![item("opaque/root", chat::Role::User)],
            turn_outcomes: Arc::new(TurnOutcomeProjection {
                revision: 1,
                complete: true,
                records: vec![],
                losses: vec![loss_for_test("native-session", &["later-part"])],
            }),
            ..Default::default()
        };
        let alias = |raw: &str| (raw == "later-part").then_some("opaque/root");
        assert!(
            project(&snapshot).is_empty(),
            "ID spelling never supplies a root"
        );
        assert_eq!(
            project_with_alias(&snapshot, alias)["opaque/root"].tone,
            Tone::ObservationEnded
        );
        assert_ne!(
            window_signature(&snapshot),
            window_signature_with_alias(&snapshot, alias)
        );
        snapshot.items[0] = item("opaque/root", chat::Role::Assistant);
        assert!(project_with_alias(&snapshot, alias).is_empty());
        snapshot.items[0] = item("opaque/root", chat::Role::User);
        let mut other = loss_for_test("native-session", &["other-part"]);
        other.accepted.input.id = "accepted-B".into();
        Arc::make_mut(&mut snapshot.turn_outcomes)
            .losses
            .push(other);
        let ambiguous =
            |raw: &str| matches!(raw, "later-part" | "other-part").then_some("opaque/root");
        assert!(
            project_with_alias(&snapshot, ambiguous).is_empty(),
            "two distinct input owners cannot share a rendered root"
        );
    }

    #[test]
    fn attention_native_terminal_proof_supersedes_only_its_exact_observation_loss() {
        for native_turn in [false, true] {
            let mut loss = loss_for_test("native-session", &["input-part"]);
            let mut terminal = record("accepted-A", &["terminal-item"], ActivityOutcome::Cancelled);
            if native_turn {
                terminal.outcome.identity.kind = TurnIdentityKind::NativeTurn;
                loss.accepted.native_turn = Some(terminal.outcome.identity.clone());
            }
            let mut snapshot = chat::Snapshot {
                session_id: Some("native-session".into()),
                items: vec![
                    item("terminal-item", chat::Role::User),
                    item("input-part", chat::Role::User),
                ],
                turn_outcomes: Arc::new(TurnOutcomeProjection {
                    revision: 2,
                    complete: true,
                    records: vec![terminal],
                    losses: vec![loss],
                }),
                ..Default::default()
            };
            snapshot.turn_outcomes.validate().unwrap();
            let markers = project(&snapshot);
            assert_eq!(markers.len(), 1);
            assert_eq!(markers["terminal-item"].tone, Tone::Cancelled);
            assert!(!markers.contains_key("input-part"));
            if native_turn {
                Arc::make_mut(&mut snapshot.turn_outcomes).losses[0]
                    .accepted
                    .native_turn = None;
                assert_eq!(
                    project(&snapshot).len(),
                    2,
                    "the same ID spelling without a captured typed turn relation proves no resolution"
                );
            }
        }
    }

    #[test]
    fn attention_malformed_simultaneous_terminal_and_loss_alias_is_not_a_valid_projection_delta() {
        let snapshot = chat::Snapshot {
            session_id: Some("native-session".into()),
            items: vec![item("input", chat::Role::User)],
            turn_outcomes: Arc::new(TurnOutcomeProjection {
                revision: 2,
                complete: true,
                records: vec![record("accepted-A", &["input"], ActivityOutcome::Succeeded)],
                losses: vec![loss_for_test("native-session", &["input"])],
            }),
            ..Default::default()
        };
        assert!(snapshot.turn_outcomes.validate().is_err());
        assert!(
            project(&snapshot).is_empty(),
            "producer must suppress resolved loss atomically before publishing the terminal alias"
        );
    }
}
