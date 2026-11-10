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
                for (index, (name, fields)) in old.iter().enumerate() {
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
                if new.len() > old.len() && direction == Direction::Reply && gate.is_none() {
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
) -> Vec<String> {
    let mut comparison = Comparison {
        older,
        current,
        gated,
        refused,
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
    let mut changed = differences(&recorded, &current, Vec::new(), Vec::new());
    changed.extend(differences(&current, &recorded, Vec::new(), Vec::new()));
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
            (client.protocol < FIRING_WITH_SKIPPED_OCCURRENCES)
                .then_some((AUTOMATION_RESPONSE, FIRING_REPLIES))
                .into_iter()
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

#[test]
fn removed_channel_work_tags_are_incompatible_with_published_older_clients() {
    let current = traced();
    for protocol in [super::super::CANDIDATE_LINE_PROTOCOL, 160] {
        assert_eq!(older_client(protocol), None);
        let changed = differences(&recorded(protocol), &current, Vec::new(), Vec::new());
        for removed in ["ChannelWork", "channel_work", "ChannelWorkChanged"] {
            assert!(
                changed
                    .iter()
                    .any(|difference| difference.contains(removed)),
                "protocol {protocol} should expose removed {removed}: {changed:?}"
            );
        }
    }
}

#[test]
fn pre_removal_protocol_preserves_the_restricted_upgrade_and_inspection_subset() {
    let older = recorded(super::super::PRE_CHANNEL_WORK_REMOVAL_PROTOCOL);
    let current = traced();
    let mut comparison = Comparison {
        older: &older,
        current: &current,
        gated: Vec::new(),
        refused: Vec::new(),
        visited: BTreeSet::new(),
        differences: Vec::new(),
    };
    // The source client sends its own protocol number, but the envelope's
    // scalar fields and ownership proof must keep their exact positions.
    for envelope in ["ClientEnvelope", "ServerEnvelope"] {
        let name = format!("cagents::terminal::ipc::{envelope}");
        let Some(Container::Struct(old)) = older.containers.get(&name) else {
            panic!("missing historical {envelope}");
        };
        let Some(Container::Struct(new)) = current.containers.get(&name) else {
            panic!("missing current {envelope}");
        };
        comparison.named(Direction::Reply, &old[..3], &new[..3], envelope);
        assert_eq!(old[3].0, new[3].0);
    }
    // Compare only messages this upgrade path exchanges, preserving each
    // original enum position and recursively comparing its complete payload.
    // Both directions are exact: a current writer must not send an appended
    // nested variant to a historical source, or misread one of its replies.
    // Comparing all requests would correctly reject the removed channel work;
    // ordinary client admission remains separate and refuses this protocol.
    for (name, direction, variants) in [
        (
            "ClientRequest",
            Direction::Reply,
            &[
                "Connect",
                "List",
                "ChatList",
                "UpgradeImpact",
                "PrepareConfirmedUpgrade",
                "ResolveUpgradeChat",
                "ChatAttach",
                "ChatDetach",
                "ChatVisibility",
                "ChatRead",
                "ChatClose",
            ][..],
        ),
        (
            "ServerResponse",
            Direction::Reply,
            &[
                "connected",
                "listed",
                "chat_listed",
                "upgrade_impact",
                "upgrade_prepared",
                "upgrade_chat_resolved",
                "chat_attached",
                "detached",
                "accepted",
                "chat_snapshot",
                "chat_closed",
            ][..],
        ),
        (
            "ServerMessage",
            Direction::Reply,
            &[
                "host_shutting_down",
                "error",
                "chat_activity",
                "chat_observed",
                "chat_changed",
                "chat_account_usage",
                "chat_limit",
            ][..],
        ),
    ] {
        let path = format!("cagents::terminal::ipc::{name}");
        let Some(Container::Enum(old)) = older.containers.get(&path) else {
            panic!("missing historical {name}");
        };
        let Some(Container::Enum(new)) = current.containers.get(&path) else {
            panic!("missing current {name}");
        };
        for variant in variants {
            let index = old.iter().position(|(name, _)| name == variant).unwrap();
            let (new_name, new_fields) = new.get(index).unwrap();
            assert_eq!(new_name, variant, "{name}::{variant} changed its tag");
            comparison.fields(
                direction,
                old[index].1.as_ref().unwrap(),
                new_fields.as_ref().unwrap(),
                &format!("{name}::{variant}"),
            );
        }
    }
    // Response is the envelope around each explicitly selected reply above.
    for shapes in [&older, &current] {
        let Some(Container::Enum(messages)) = shapes
            .containers
            .get("cagents::terminal::ipc::ServerMessage")
        else {
            unreachable!();
        };
        assert_eq!(messages[0].0, "response");
        assert!(matches!(
            &messages[0].1,
            Some(Fields::Struct(fields)) if fields == &vec![(
                "response".into(),
                Shape::Container("cagents::terminal::ipc::ServerResponse".into())
            )]
        ));
    }
    assert!(
        comparison.differences.is_empty(),
        "{:?}",
        comparison.differences
    );
}
