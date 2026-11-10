//! The recorded postcard wire format of each protocol, and the proof that an
//! older client this daemon serves reads and writes it as this one does.
//!
//! `tests/fixtures/wire-formats/<protocol>.json` holds the shape of every type
//! reachable from the client and server envelopes, as [`Shapes::trace`] finds
//! it. After a protocol bump, record the new protocol by running
//! `this_protocols_wire_format_is_recorded` with `CAGENTS_RECORD_WIRE_FORMAT=1`,
//! and keep the older files. Protocol 128 was recorded by running this
//! tracer in a 0.1.3+local.c68275e94 checkout.

use std::{collections::BTreeSet, path::PathBuf};

use super::{
    wire_shape::{Container, Fields, Shape, Shapes},
    *,
};
use crate::terminal_ipc::{ClientEnvelope, ServerEnvelope};

const RECORD: &str = "CAGENTS_RECORD_WIRE_FORMAT";

/// By full name, because other enums are also called `Response`.
const WORK_RESPONSE: &str = "cagents::work::Response";
const AGENT_REPLY: &str = "cagents::agent_execution::Reply";
const AUTOMATION_RESPONSE: &str = "cagents::automations::Response";

fn fixture(protocol: u16) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wire-formats")
        .join(format!("{protocol}.json"))
}

fn recorded(protocol: u16) -> Shapes {
    let path = fixture(protocol);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn traced() -> Shapes {
    Shapes::trace::<ClientEnvelope, ServerEnvelope>().unwrap_or_else(|error| panic!("{error}"))
}

/// Which side encodes a position.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Direction {
    /// The older side encodes; this side may have appended enum variants.
    Request,
    /// This side encodes; both sides must agree exactly.
    Reply,
}

/// Compares positions by shape, field and variant names. Container names are
/// not compared: postcard never encodes them, and types move between modules.
struct Comparison<'a> {
    older: &'a Shapes,
    current: &'a Shapes,
    /// Enums whose later variants the connection withholds, by short name
    /// (`ServerMessage`) or full path (`cagents::work::Response`), with the
    /// older client's variant count.
    gated: Vec<(&'static str, usize)>,
    /// Variants of the named enum that the connection refuses, so their
    /// contents need not match. Their positions still must.
    refused: Vec<(&'static str, &'static [&'static str])>,
    /// Appended variants of the named enum that the older client is sent as
    /// one it knows.
    restated: Vec<(&'static str, &'static str)>,
    /// When present, only these outer variants belong to the proven contract.
    /// Nested enums remain unrestricted and must agree in full.
    selected: &'static [(&'static str, &'static [&'static str])],
    visited: BTreeSet<(Direction, String, String)>,
    differences: Vec<String>,
}

fn short(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

impl Comparison<'_> {
    fn differ(&mut self, path: &str, what: String) {
        self.differences.push(format!("{path}: {what}"));
    }

    fn container(&mut self, direction: Direction, older: &str, current: &str, path: &str) {
        if !self
            .visited
            .insert((direction, older.to_owned(), current.to_owned()))
        {
            return;
        }
        let (Some(old), Some(new)) = (
            self.older.containers.get(older),
            self.current.containers.get(current),
        ) else {
            return self.differ(path, format!("{older} or {current} was not recorded"));
        };
        match (old, new) {
            (Container::UnitStruct, Container::UnitStruct) => {}
            (Container::Newtype(old), Container::Newtype(new)) => {
                self.shape(direction, old, new, path);
            }
            (Container::TupleStruct(old), Container::TupleStruct(new)) => {
                self.shapes(direction, old, new, path);
            }
            (Container::Struct(old), Container::Struct(new)) => {
                self.named(direction, old, new, path);
            }
            (Container::Enum(old), Container::Enum(new)) => {
                let gate = self
                    .gated
                    .iter()
                    .find(|(name, _)| *name == short(current) || *name == current)
                    .map(|(_, count)| *count);
                if let Some(count) = gate
                    && old.len() != count
                {
                    self.differ(path, format!("the older client has {} variants", old.len()));
                }
                let selected = self
                    .selected
                    .iter()
                    .find(|(name, _)| *name == short(current) || *name == current)
                    .map(|(_, variants)| *variants);
                if let Some(selected) = selected {
                    for name in selected {
                        if !old.iter().any(|(variant, _)| variant == name)
                            || !new.iter().any(|(variant, _)| variant == name)
                        {
                            self.differ(path, format!("required variant {name} was not recorded"));
                        }
                    }
                }
                for (index, (name, fields)) in old.iter().enumerate() {
                    if selected.is_some_and(|variants| !variants.contains(&name.as_str())) {
                        continue;
                    }
                    let path = format!("{path}::{name}");
                    let Some((next, next_fields)) = new.get(index) else {
                        self.differ(&path, "variant removed".into());
                        continue;
                    };
                    if name != next {
                        self.differ(&path, format!("variant {index} is now {next}"));
                    }
                    if self.refused.iter().any(|(enum_name, variants)| {
                        *enum_name == current && variants.contains(&name.as_str())
                    }) {
                        continue;
                    }
                    match (fields, next_fields) {
                        (Some(old), Some(new)) => self.fields(direction, old, new, &path),
                        _ => self.differ(&path, "variant was not traced".into()),
                    }
                }
                let appended = new.iter().skip(old.len()).any(|(name, _)| {
                    !self
                        .restated
                        .iter()
                        .any(|restated| *restated == (current, name.as_str()))
                });
                if appended && direction == Direction::Reply && gate.is_none() && selected.is_none()
                {
                    self.differ(
                        path,
                        "appended variants the older client cannot decode".into(),
                    );
                }
            }
            _ => self.differ(path, "container kind changed".into()),
        }
    }

    fn fields(&mut self, direction: Direction, old: &Fields, new: &Fields, path: &str) {
        match (old, new) {
            (Fields::Unit, Fields::Unit) => {}
            (Fields::Newtype(old), Fields::Newtype(new)) => self.shape(direction, old, new, path),
            (Fields::Tuple(old), Fields::Tuple(new)) => self.shapes(direction, old, new, path),
            (Fields::Struct(old), Fields::Struct(new)) => self.named(direction, old, new, path),
            _ => self.differ(path, "variant kind changed".into()),
        }
    }

    fn named(
        &mut self,
        direction: Direction,
        old: &[(String, Shape)],
        new: &[(String, Shape)],
        path: &str,
    ) {
        if old.len() != new.len() {
            return self.differ(path, format!("{} fields, now {}", old.len(), new.len()));
        }
        for ((name, old), (next, new)) in old.iter().zip(new) {
            let path = format!("{path}.{name}");
            if name != next {
                self.differ(&path, format!("field is now {next}"));
            }
            self.shape(direction, old, new, &path);
        }
    }

    fn shapes(&mut self, direction: Direction, old: &[Shape], new: &[Shape], path: &str) {
        if old.len() != new.len() {
            return self.differ(path, format!("{} elements, now {}", old.len(), new.len()));
        }
        for (index, (old, new)) in old.iter().zip(new).enumerate() {
            self.shape(direction, old, new, &format!("{path}.{index}"));
        }
    }

    fn shape(&mut self, direction: Direction, old: &Shape, new: &Shape, path: &str) {
        match (old, new) {
            (Shape::Container(old), Shape::Container(new)) => {
                self.container(direction, old, new, path);
            }
            (Shape::Option(old), Shape::Option(new)) | (Shape::Seq(old), Shape::Seq(new)) => {
                self.shape(direction, old, new, path);
            }
            (Shape::Map(old_key, old_value), Shape::Map(new_key, new_value)) => {
                self.shape(direction, old_key, new_key, &format!("{path}[key]"));
                self.shape(direction, old_value, new_value, &format!("{path}[value]"));
            }
            (Shape::Tuple(old), Shape::Tuple(new)) => self.shapes(direction, old, new, path),
            (Shape::Unknown, _) | (_, Shape::Unknown) => {
                self.differ(path, "not traced".into());
            }
            (old, new) if old == new => {}
            (old, new) => self.differ(path, format!("{old:?} is now {new:?}")),
        }
    }
}

fn differences(
    older: &Shapes,
    current: &Shapes,
    gated: Vec<(&'static str, usize)>,
    refused: Vec<(&'static str, &'static [&'static str])>,
    restated: Vec<(&'static str, &'static str)>,
) -> Vec<String> {
    let mut comparison = Comparison {
        older,
        current,
        gated,
        refused,
        restated,
        selected: &[],
        visited: BTreeSet::new(),
        differences: Vec::new(),
    };
    comparison.shape(
        Direction::Request,
        &older.client,
        &current.client,
        "ClientEnvelope",
    );
    comparison.shape(
        Direction::Reply,
        &older.server,
        &current.server,
        "ServerEnvelope",
    );
    comparison.differences
}

fn variant_count(shapes: &Shapes, name: &str) -> u32 {
    let mut enums = shapes
        .containers
        .iter()
        .filter(|(key, _)| short(key) == name || key.as_str() == name)
        .filter_map(|(_, container)| match container {
            Container::Enum(variants) => Some(variants.len() as u32),
            _ => None,
        });
    let count = enums
        .next()
        .unwrap_or_else(|| panic!("{name} was not recorded"));
    assert!(enums.next().is_none(), "more than one {name} was recorded");
    count
}

#[test]
fn this_protocols_wire_format_is_recorded() {
    let current = traced();
    if std::env::var_os(RECORD).is_some() {
        // Written to the checkout the test ran in, and printed for a build
        // host whose checkout is a copy.
        let path = fixture(PROTOCOL_VERSION);
        let text = serde_json::to_string_pretty(&current).unwrap() + "\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        println!("{RECORD} {}\n{text}{RECORD} end", path.display());
        return;
    }
    let recorded = recorded(PROTOCOL_VERSION);
    let mut changed = differences(&recorded, &current, Vec::new(), Vec::new(), Vec::new());
    changed.extend(differences(
        &current,
        &recorded,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ));
    assert!(
        changed.is_empty(),
        "the wire format differs from protocol {PROTOCOL_VERSION}'s recording. Postcard is \
         positional, so bump PROTOCOL_VERSION, record the new protocol with {RECORD}=1, and \
         prove or retire each OlderClient:\n{}",
        changed.join("\n")
    );
}

#[test]
fn every_older_client_is_proven_against_its_recorded_format() {
    let current = traced();
    for client in OLDER_CLIENTS {
        assert_eq!(
            client.proven_at, PROTOCOL_VERSION,
            "protocol {} clients were proven compatible with protocol {}, not {PROTOCOL_VERSION}. \
             After a bump, check that its requests still mean what they did, then update \
             `proven_at`, or remove it from OLDER_CLIENTS.",
            client.protocol, client.proven_at
        );
        let older = recorded(client.protocol);
        assert_eq!(
            (
                client.client_requests,
                client.server_messages,
                client.server_responses,
                client.work_responses,
                client.agent_replies
            ),
            (
                variant_count(&older, "ClientRequest"),
                variant_count(&older, "ServerMessage"),
                variant_count(&older, "ServerResponse"),
                variant_count(&older, WORK_RESPONSE),
                variant_count(&older, AGENT_REPLY),
            ),
            "protocol {}'s variant counts",
            client.protocol
        );
        let differences = differences(
            &older,
            &current,
            vec![
                ("ServerMessage", client.server_messages as usize),
                ("ServerResponse", client.server_responses as usize),
                (WORK_RESPONSE, client.work_responses as usize),
                (AGENT_REPLY, client.agent_replies as usize),
            ],
            if client.protocol < FIRING_WITH_SKIPPED_OCCURRENCES {
                vec![(AUTOMATION_RESPONSE, FIRING_REPLIES)]
            } else {
                Vec::new()
            },
            RESTATED_VARIANTS
                .iter()
                .filter(|(.., since)| client.protocol < *since)
                .map(|(container, variant, _)| (*container, *variant))
                .collect(),
        );
        assert!(
            differences.is_empty(),
            "protocol {} clients would misread this daemon. Adapt or refuse what changed for \
             them, or remove them from OLDER_CLIENTS:\n{}",
            client.protocol,
            differences.join("\n")
        );
    }
}

/// Compare lifecycle and inspection in both directions: unlike general older
/// clients, nested enums may neither add nor remove variants. Both envelope
/// headers are included, so proof fields and request correlation stay frozen.
fn lifecycle_differences(older: &Shapes, current: &Shapes) -> Vec<String> {
    let mut changed = Vec::new();
    for (older, current) in [(older, current), (current, older)] {
        let mut comparison = Comparison {
            older,
            current,
            gated: Vec::new(),
            refused: Vec::new(),
            restated: Vec::new(),
            selected: crate::update::source::LIFECYCLE_VARIANTS,
            visited: BTreeSet::new(),
            differences: Vec::new(),
        };
        comparison.shape(
            Direction::Request,
            &older.client,
            &current.client,
            "ClientEnvelope",
        );
        comparison.shape(
            Direction::Reply,
            &older.server,
            &current.server,
            "ServerEnvelope",
        );
        changed.extend(comparison.differences);
    }
    changed
}

#[test]
fn every_current_client_update_source_preserves_the_frozen_lifecycle_and_inspector_wire_format() {
    let current = traced();
    let current_recording = recorded(PROTOCOL_VERSION);
    for protocol in crate::update::source::current_client_protocols() {
        let older = recorded(protocol);
        let mut changed = lifecycle_differences(&older, &current_recording);
        // Also compare actual Rust types: regenerating the current fixture
        // cannot bless an incompatible change to an admitted source's subset.
        changed.extend(lifecycle_differences(&older, &current));
        assert!(
            changed.is_empty(),
            "update source protocol {protocol} no longer has this build's frozen lifecycle and inspector wire format:\n{}",
            changed.join("\n")
        );
    }
}

#[test]
fn inspector_proof_detects_each_request_response_and_reachable_history_change() {
    // Both recorded source lines must prove the inspector as well as lifecycle
    // control. These mutations also catch accidentally narrowing the subset.
    for protocol in [128, 156] {
        let baseline = recorded(protocol);
        for (container, names) in [
            (
                "ClientRequest",
                &[
                    "ChatStart",
                    "ResolveUpgradeChat",
                    "ChatVisibility",
                    "ChatRead",
                ][..],
            ),
            (
                "ServerResponse",
                &["accepted", "chat_snapshot", "upgrade_chat_resolved"][..],
            ),
        ] {
            let key = baseline
                .containers
                .keys()
                .find(|key| short(key) == container)
                .unwrap();
            for name in names {
                let mut changed = baseline.clone();
                let Container::Enum(variants) = changed.containers.get_mut(key).unwrap() else {
                    panic!()
                };
                let index = variants
                    .iter()
                    .position(|(variant, _)| variant == name)
                    .unwrap();
                variants.swap(index, index - 1);
                assert!(
                    !lifecycle_differences(&baseline, &changed).is_empty(),
                    "protocol {protocol}: inspector variant {name} must be frozen"
                );
            }
        }

        let mut changed = baseline.clone();
        let Container::Struct(fields) = changed
            .containers
            .get_mut("cagents::chat::Snapshot")
            .unwrap()
        else {
            panic!()
        };
        fields.push(("future_history_field".into(), Shape::Bool));
        assert!(!lifecycle_differences(&baseline, &changed).is_empty());
    }
}

#[test]
fn lifecycle_proof_detects_variant_positions_and_reachable_payload_changes() {
    let baseline = recorded(128);
    let key = |name: &str| {
        baseline
            .containers
            .keys()
            .find(|key| short(key) == name)
            .unwrap()
            .clone()
    };
    let mut changed = baseline.clone();
    let Container::Enum(variants) = changed.containers.get_mut(&key("ClientRequest")).unwrap()
    else {
        panic!()
    };
    variants.swap(0, 1);
    assert!(!lifecycle_differences(&baseline, &changed).is_empty());

    let mut changed = baseline.clone();
    let Container::Struct(fields) = changed.containers.get_mut(&key("DaemonImpact")).unwrap()
    else {
        panic!()
    };
    fields.push(("future_field".into(), Shape::Bool));
    assert!(!lifecycle_differences(&baseline, &changed).is_empty());

    let mut changed = baseline.clone();
    let Container::Enum(variants) = changed.containers.get_mut(&key("Disposition")).unwrap() else {
        panic!()
    };
    variants.push(("future_variant".into(), Some(Fields::Unit)));
    assert!(!lifecycle_differences(&baseline, &changed).is_empty());

    // The newer daemon can evolve unrelated agent operations independently.
    let mut changed = baseline.clone();
    let Container::Enum(variants) = changed.containers.get_mut(AGENT_REPLY).unwrap() else {
        panic!()
    };
    variants.push(("unrelated_variant".into(), Some(Fields::Unit)));
    assert!(lifecycle_differences(&baseline, &changed).is_empty());
}
