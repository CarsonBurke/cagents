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
const SERVER_MESSAGE: &str = "cagents::terminal::ipc::ServerMessage";

#[test]
fn workspace_delta_requests_and_receipts_round_trip_as_framed_messages() {
    use crate::{
        model::{CardId, ChannelId, HostSessionProof},
        terminal_ipc::{read_frame, write_frame},
        workspace_model::{
            ModelEdit,
            delta::{EditEffects, EditReceipt, ViewRevision},
            views::{Change, Scope},
        },
    };

    let scope = Scope::Channel(ChannelId::new());
    let closed = CardId::new();
    let edit = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: 7,
        operation: uuid::Uuid::new_v4(),
        change: Change::Remove { id: closed },
    };
    let proof = HostSessionProof {
        host_session_id: "workspace-wire".into(),
        boot_id: "boot".into(),
        host_incarnation: "incarnation".into(),
    };
    for request in [
        ClientRequest::ReadWorkspaceMetadata {
            query: crate::workspace_model::metadata::Query::Identity,
        },
        ClientRequest::ReadWorkspaceMetadata {
            query: crate::workspace_model::metadata::Query::Projects,
        },
        ClientRequest::ReadWorkspaceMetadata {
            query: crate::workspace_model::metadata::Query::Agents,
        },
        ClientRequest::ReadWorkspaceSnapshot,
        ClientRequest::ReadWorkspaceView {
            scope: scope.clone(),
        },
        ClientRequest::ApplyWorkspaceEdit { edit },
    ] {
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 41,
                proof: Some(proof.clone()),
                request: request.clone(),
            },
        )
        .unwrap();
        let decoded: ClientEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!(decoded.request, request);
        assert_eq!(decoded.request_id, 41);
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.proof, Some(proof.clone()));
    }

    use crate::workspace_model::metadata::Reply;
    let identity = uuid::Uuid::new_v4();
    for metadata in [
        Reply::Identity(identity),
        Reply::Projects {
            identity,
            projects: vec![],
        },
        Reply::Agents {
            identity,
            agents: vec![],
        },
    ] {
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(41),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::WorkspaceMetadata(metadata.clone()),
                },
            },
        )
        .unwrap();
        let decoded: ServerEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!(decoded.proof, proof);
        assert_eq!(decoded.request_id, Some(41));
        let ServerMessage::Response {
            response: ServerResponse::WorkspaceMetadata(decoded),
        } = decoded.message
        else {
            panic!("metadata response")
        };
        assert_eq!(decoded, metadata);
    }

    let receipt = EditReceipt {
        identity: uuid::Uuid::new_v4(),
        sequence: 12,
        effects: EditEffects {
            views: vec![ViewRevision {
                scope,
                before: Some(7),
                after: Some(12),
            }],
            closed: vec![closed],
            ..Default::default()
        },
    };
    let mut frame = Vec::new();
    write_frame(
        &mut frame,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(41),
            proof,
            message: ServerMessage::Response {
                response: ServerResponse::WorkspaceEdit(receipt.clone()),
            },
        },
    )
    .unwrap();
    let decoded: ServerEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
    assert_eq!(decoded.version, PROTOCOL_VERSION);
    assert_eq!(decoded.request_id, Some(41));
    let ServerMessage::Response {
        response: ServerResponse::WorkspaceEdit(decoded),
    } = decoded.message
    else {
        panic!("a correlated workspace delta receipt was expected");
    };
    assert_eq!(decoded, receipt);
}

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
                        // Compared the other way round, an appended variant
                        // the daemon restates is one that side never sends.
                        if !self.restated.contains(&(current, name.as_str())) {
                            self.differ(&path, "variant removed".into());
                        }
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
        // Written to the checkout the test ran in; scripts/cargo-gate brings
        // it back from a build host's copy. Printed as well, for a runner
        // that does not.
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
            [
                (client.protocol < FIRING_WITH_SKIPPED_OCCURRENCES)
                    .then_some((AUTOMATION_RESPONSE, FIRING_REPLIES)),
                (client.protocol < MODEL_EDGES_WITH_EFFECTS)
                    .then_some((SERVER_MESSAGE, MODEL_EDGES)),
                (client.protocol < WORK_CHECK_COUNTS)
                    .then_some((WORK_RESPONSE, WORK_CHECK_REPLIES)),
                (client.protocol < WORK_CHECK_COUNTS)
                    .then_some((SERVER_MESSAGE, WORK_BOARD_MESSAGES)),
            ]
            .into_iter()
            .flatten()
            .collect(),
            restated_for(client.protocol),
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

/// Appended variants a client of `protocol` is sent as ones it knows.
fn restated_for(protocol: u16) -> Vec<(&'static str, &'static str)> {
    RESTATED_VARIANTS
        .iter()
        .filter(|(.., since)| protocol < *since)
        .map(|(container, variant, _)| (*container, *variant))
        .collect()
}

/// Compare lifecycle and inspection in both directions: unlike general older
/// clients, nested enums may neither add nor remove variants. Both envelope
/// headers are included, so proof fields and request correlation stay frozen.
/// A variant in `restated` is sent to that older side as one it knows, so it
/// is neither an addition it can't decode nor a removal.
fn lifecycle_differences(
    older: &Shapes,
    current: &Shapes,
    restated: &[(&'static str, &'static str)],
) -> Vec<String> {
    let mut changed = Vec::new();
    for (older, current) in [(older, current), (current, older)] {
        let mut comparison = Comparison {
            older,
            current,
            gated: Vec::new(),
            refused: Vec::new(),
            restated: restated.to_vec(),
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
        let restated = restated_for(protocol);
        let mut changed = lifecycle_differences(&older, &current_recording, &restated);
        // Also compare actual Rust types: regenerating the current fixture
        // cannot bless an incompatible change to an admitted source's subset.
        changed.extend(lifecycle_differences(&older, &current, &restated));
        assert!(
            changed.is_empty(),
            "update source protocol {protocol} no longer has this build's frozen lifecycle and inspector wire format:\n{}",
            changed.join("\n")
        );
    }
}

#[test]
fn inspector_proof_detects_each_request_response_and_reachable_history_change() {
    // Every admitted Current source must prove the inspector as well as
    // lifecycle control. These mutations catch narrowing the subset.
    for protocol in crate::update::source::current_client_protocols() {
        let baseline = recorded(protocol);
        for (container, names) in [
            (
                "ClientRequest",
                &[
                    "ChatStart",
                    "ResolveUpgradeChat",
                    "ChatVisibility",
                    "ChatRead",
                    "StartupState",
                ][..],
            ),
            (
                "ServerResponse",
                &[
                    "accepted",
                    "chat_snapshot",
                    "upgrade_chat_resolved",
                    "startup_state",
                ][..],
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
                    !lifecycle_differences(&baseline, &changed, &[]).is_empty(),
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
        assert!(!lifecycle_differences(&baseline, &changed, &[]).is_empty());
    }
}

#[test]
fn readiness_proof_detects_startup_phase_variants_and_nested_payload_changes() {
    for protocol in crate::update::source::current_client_protocols() {
        let baseline = recorded(protocol);
        let key = baseline
            .containers
            .keys()
            .find(|key| short(key) == "StartupState")
            .unwrap();
        for mutation in [
            "variant_position",
            "new_phase",
            "restoring_count",
            "retained_detail",
        ] {
            let mut changed = baseline.clone();
            let Container::Enum(variants) = changed.containers.get_mut(key).unwrap() else {
                panic!("StartupState must be an enum");
            };
            match mutation {
                "variant_position" => variants.swap(0, 1),
                "new_phase" => variants.push(("future_phase".into(), Some(Fields::Unit))),
                "restoring_count" | "retained_detail" => {
                    let name = if mutation == "restoring_count" {
                        "restoring"
                    } else {
                        "retained"
                    };
                    let (_, Some(Fields::Struct(fields))) = variants
                        .iter_mut()
                        .find(|(variant, _)| variant == name)
                        .unwrap()
                    else {
                        panic!("{name} must retain its named payload");
                    };
                    fields.push(("incompatible_readiness_field".into(), Shape::Bool));
                }
                _ => unreachable!(),
            }
            assert!(
                !lifecycle_differences(&baseline, &changed, &[]).is_empty(),
                "protocol {protocol}: readiness mutation {mutation} must be rejected"
            );
        }
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
    assert!(!lifecycle_differences(&baseline, &changed, &[]).is_empty());

    let mut changed = baseline.clone();
    let Container::Struct(fields) = changed.containers.get_mut(&key("DaemonImpact")).unwrap()
    else {
        panic!()
    };
    fields.push(("future_field".into(), Shape::Bool));
    assert!(!lifecycle_differences(&baseline, &changed, &[]).is_empty());

    let mut changed = baseline.clone();
    let Container::Enum(variants) = changed.containers.get_mut(&key("Disposition")).unwrap() else {
        panic!()
    };
    variants.push(("future_variant".into(), Some(Fields::Unit)));
    assert!(!lifecycle_differences(&baseline, &changed, &[]).is_empty());

    // The newer daemon can evolve unrelated agent operations independently.
    let mut changed = baseline.clone();
    let Container::Enum(variants) = changed.containers.get_mut(AGENT_REPLY).unwrap() else {
        panic!()
    };
    variants.push(("unrelated_variant".into(), Some(Fields::Unit)));
    assert!(lifecycle_differences(&baseline, &changed, &[]).is_empty());
}

/// How a provider addresses a chat's subagents travels beside its snapshot,
/// whose layout every admitted update source shares, so neither an older
/// listed client nor an older source daemon could be sent or send it.
#[test]
fn agent_addresses_are_beyond_every_older_recording() {
    let addresses = ServerMessage::ChatAgentAddresses {
        chat_id: crate::model::TerminalId::new(),
        addresses: Vec::new(),
    };
    let index = super::variant_index(&addresses).unwrap();
    assert_eq!(index + 1, variant_count(&traced(), "ServerMessage"));
    let older = OLDER_CLIENTS
        .iter()
        .map(|client| client.protocol)
        .chain(crate::update::source::current_client_protocols())
        .filter(|protocol| *protocol < PROTOCOL_VERSION);
    for protocol in older {
        assert!(
            variant_count(&recorded(protocol), "ServerMessage") <= index,
            "protocol {protocol} already had a message at its position"
        );
    }
}
