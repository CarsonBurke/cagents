//! Holds the mobile API to its promise that, within a version, it only grows.
//!
//! `crates/mobile-api/fixtures/v1.jsonl` records one message of every shape
//! the API has had. Each recorded message must still decode, and encoding
//! what it decodes to must say at least everything it said: a removed or
//! renamed field, a renamed enum value or a changed tag fails here. The phone
//! app's own tests can read the same file.
//!
//! A new shape fails [`every_example_is_recorded`] until it is recorded:
//! `CAGENTS_RECORD_MOBILE_API=1` appends examples that have no recording.
//! Recordings are never rewritten.

// Nothing here depends on a feature, so only the cli lane runs this target.
#![cfg(not(any(feature = "host", feature = "ui")))]

use std::{collections::BTreeMap, path::PathBuf};

use cagents_mobile_api::*;
use serde_json::{Value, json};

const RECORD: &str = "CAGENTS_RECORD_MOBILE_API";

fn recordings_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/mobile-api/fixtures/v1.jsonl")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    Client,
    Server,
}

impl Direction {
    fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }

    /// Decode `message` as this direction's line and encode it again.
    fn round_trip(self, message: &Value) -> Result<Value, serde_json::Error> {
        match self {
            Self::Client => {
                serde_json::from_value::<ClientLine>(message.clone()).map(|line| json!(line))
            }
            Self::Server => {
                serde_json::from_value::<ServerLine>(message.clone()).map(|line| json!(line))
            }
        }
    }
}

fn request(id: u64, call: Call) -> (Direction, Value) {
    (
        Direction::Client,
        json!(ClientLine::Request(Request { id, call })),
    )
}

fn server(line: ServerLine) -> (Direction, Value) {
    (Direction::Server, json!(line))
}

fn event(event: Event) -> (Direction, Value) {
    server(ServerLine::Event(event))
}

fn choice(id: &str) -> Choice {
    Choice {
        id: id.into(),
        label: id.into(),
        description: String::new(),
    }
}

fn state() -> ConversationState {
    ConversationState {
        title: Some("Fix the build".into()),
        activity: Activity::Waiting,
        busy: true,
        ready: true,
        exited: false,
        error: Some("Usage limit reached".into()),
        read_only: Some("Saved history".into()),
        has_earlier: true,
        loading_earlier: false,
        model: Some("opus".into()),
        requests: vec![PendingRequest {
            id: "r1".into(),
            kind: RequestKind::Approval,
            title: "Run cargo test?".into(),
            description: "cargo test".into(),
            fields: vec![RequestField {
                id: "reason".into(),
                label: "Reason".into(),
                description: String::new(),
                choices: vec![choice("ci")],
                multiple: false,
                secret: false,
                initial: String::new(),
            }],
            actions: vec![choice("allow"), choice("deny")],
        }],
        queued: vec![QueuedMessage {
            id: "q1".into(),
            text: "Also run clippy".into(),
        }],
        plan: vec![PlanStep {
            text: "Reproduce".into(),
            status: PlanStatus::Active,
        }],
        subagents: vec![Subagent {
            id: "t1".into(),
            kind: "Explore".into(),
            description: "Map the code".into(),
            status: ToolStatus::Running,
            waiting: false,
        }],
    }
}

fn item(id: &str, role: Role, body: ItemBody) -> Item {
    Item {
        id: id.into(),
        role,
        body,
        started_at: Some(1),
        at: Some(2),
    }
}

/// One message of every shape, named.
fn examples() -> BTreeMap<&'static str, (Direction, Value)> {
    let session = || "8d4f2a8e-3c55-4b8e-9d2a-1f0c6b7e5a10".to_owned();
    BTreeMap::from([
        (
            "hello",
            (
                Direction::Client,
                json!(ClientLine::Hello(Hello {
                    api_min: 1,
                    api_max: 1,
                    client: ClientInfo {
                        name: "cagents-android".into(),
                        version: "1.0.0".into(),
                    },
                })),
            ),
        ),
        ("ping", request(1, Call::Ping {})),
        ("sessions.watch", request(2, Call::WatchSessions {})),
        ("sessions.unwatch", request(3, Call::UnwatchSessions {})),
        (
            "notifications.watch",
            request(4, Call::WatchNotifications {}),
        ),
        (
            "notifications.unwatch",
            request(5, Call::UnwatchNotifications {}),
        ),
        (
            "notification.acknowledge",
            request(
                6,
                Call::AcknowledgeNotification {
                    id: "0b0e7a52-6f6b-4f7c-8a35-5c9f3f0e2d11".into(),
                },
            ),
        ),
        (
            "conversation.watch",
            request(7, Call::WatchConversation { session: session() }),
        ),
        (
            "conversation.unwatch",
            request(8, Call::UnwatchConversation { session: session() }),
        ),
        (
            "conversation.load_earlier",
            request(9, Call::LoadEarlier { session: session() }),
        ),
        (
            "conversation.send",
            request(
                10,
                Call::Send {
                    session: session(),
                    text: "Run the tests".into(),
                    mode: SendMode::Steer,
                },
            ),
        ),
        (
            "conversation.interrupt",
            request(11, Call::Interrupt { session: session() }),
        ),
        (
            "conversation.answer",
            request(
                12,
                Call::Answer {
                    session: session(),
                    request: "r1".into(),
                    action: "allow".into(),
                    answers: vec![FieldAnswer {
                        field: "reason".into(),
                        values: vec!["ci".into()],
                    }],
                },
            ),
        ),
        (
            "welcome",
            server(ServerLine::Welcome(Welcome {
                api: 1,
                server_version: "0.1.3".into(),
                host: Some("desktop".into()),
            })),
        ),
        ("ok", server(ServerLine::Ok { id: 7 })),
        (
            "error",
            server(ServerLine::Error {
                id: Some(8),
                error: Error {
                    code: ErrorCode::NotWatching,
                    message: "Open the conversation before acting on it.".into(),
                },
            }),
        ),
        (
            "error without id",
            server(ServerLine::Error {
                id: None,
                error: Error {
                    code: ErrorCode::UnsupportedApi,
                    message: "Update the app.".into(),
                },
            }),
        ),
        (
            "event sessions",
            event(Event::Sessions(Sessions {
                sessions: vec![Session {
                    id: session(),
                    lineage: session(),
                    surface: Surface::Chat,
                    provider: Some("codex".into()),
                    provider_label: Some("Codex".into()),
                    title: Some("Fix the build".into()),
                    project: Some("cagents".into()),
                    directory: Some("/home/person/cagents".into()),
                    activity: Activity::Working,
                    exited: false,
                    failure: Some("Usage limit reached".into()),
                    active_at: Some(1_700_000_000_000),
                    agent: Some(Agent {
                        id: "5e3c1d2a-7b8f-4a6e-9c0d-2f1e3a4b5c6d".into(),
                        owner: AgentOwner::Session,
                        owner_lineage: Some(session()),
                        run: Some(RunState::Running),
                    }),
                    watchable: true,
                }],
            })),
        ),
        (
            "event conversation",
            event(Event::Conversation(Box::new(ConversationUpdate {
                session: session(),
                reset: true,
                state: state(),
                order: Some(vec!["i1".into(), "i2".into()]),
                items: vec![
                    item(
                        "i1",
                        Role::User,
                        ItemBody::Text {
                            text: "Fix the build".into(),
                            thinking: false,
                            commentary: false,
                        },
                    ),
                    item(
                        "i2",
                        Role::Assistant,
                        ItemBody::Command {
                            label: "the tests".into(),
                            command: "cargo test".into(),
                            action: CommandAction::Run,
                            status: ToolStatus::Failed,
                            exit_code: Some(101),
                            duration_ms: Some(5000),
                            output: "1 failed".into(),
                            clipped: true,
                        },
                    ),
                ],
                appends: vec![TextAppend {
                    id: "i3".into(),
                    offset: 12,
                    text: " more".into(),
                    at: Some(3),
                }],
            }))),
        ),
        (
            "item changes",
            event(Event::Conversation(Box::new(ConversationUpdate {
                session: session(),
                reset: false,
                state: state(),
                order: None,
                items: vec![
                    item(
                        "c",
                        Role::Assistant,
                        ItemBody::Changes {
                            files: vec![FileChange {
                                path: "src/new.rs".into(),
                                change: ChangeKind::Renamed,
                                from: Some("src/old.rs".into()),
                                added_lines: 3,
                                removed_lines: 1,
                            }],
                            status: ToolStatus::Succeeded,
                        },
                    ),
                    item(
                        "w",
                        Role::Assistant,
                        ItemBody::WebSearch {
                            query: "serde flatten".into(),
                            results: vec![SearchResult {
                                title: "serde".into(),
                                url: "https://serde.rs".into(),
                            }],
                        },
                    ),
                    item(
                        "t",
                        Role::Assistant,
                        ItemBody::Tool {
                            name: "mcp__search".into(),
                            input: "{}".into(),
                            output: "done".into(),
                            clipped: false,
                            status: ToolStatus::Waiting,
                        },
                    ),
                    item(
                        "m",
                        Role::User,
                        ItemBody::Image {
                            description: "A screenshot".into(),
                        },
                    ),
                    item(
                        "d",
                        Role::System,
                        ItemBody::Delivery {
                            delivery: DeliveryKind::Report,
                            from: "Explore".into(),
                            sender: String::new(),
                            summary: "Agent \"Map\" finished".into(),
                            body: "Found it".into(),
                            clipped: false,
                        },
                    ),
                ],
                appends: Vec::new(),
            }))),
        ),
        (
            "event conversation session message",
            event(Event::Conversation(Box::new(ConversationUpdate {
                session: session(),
                reset: false,
                state: state(),
                order: None,
                items: vec![item(
                    "message",
                    Role::User,
                    ItemBody::Delivery {
                        delivery: DeliveryKind::Message,
                        from: "Spectra analysis".into(),
                        sender: "a3d7ff73".into(),
                        summary: String::new(),
                        body: "Which of these fit your plan?".into(),
                        clipped: false,
                    },
                )],
                appends: Vec::new(),
            }))),
        ),
        (
            "event conversation_ended",
            event(Event::ConversationEnded {
                session: session(),
                message: "The conversation closed.".into(),
            }),
        ),
        (
            "event notifications",
            event(Event::Notifications(Notifications {
                notices: vec![Notice {
                    id: "0b0e7a52-6f6b-4f7c-8a35-5c9f3f0e2d11".into(),
                    kind: NoticeKind::Waiting,
                    title: "Agent needs your attention".into(),
                    session: Some(session()),
                    conversation: Some("Fix the build".into()),
                    preview: Some("Run cargo test?".into()),
                    request: Some(RequestKind::Approval),
                    provider: Some("claude".into()),
                }],
            })),
        ),
        (
            "event closing",
            event(Event::Closing {
                message: "The computer's cagents daemon is stopping.".into(),
            }),
        ),
    ])
}

struct Recording {
    name: String,
    direction: Direction,
    message: Value,
}

fn recordings() -> Vec<Recording> {
    let text = std::fs::read_to_string(recordings_path()).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("a recording is JSON");
            Recording {
                name: value["name"].as_str().expect("named").to_owned(),
                direction: match value["direction"].as_str() {
                    Some("client") => Direction::Client,
                    Some("server") => Direction::Server,
                    other => panic!("unknown direction {other:?}"),
                },
                message: value["message"].clone(),
            }
        })
        .collect()
}

/// Whether `current` says everything `recorded` said. Objects may have
/// gained fields; nothing else may differ.
fn says_at_least(recorded: &Value, current: &Value, path: &str) -> Result<(), String> {
    match (recorded, current) {
        (Value::Object(recorded), Value::Object(current)) => {
            for (key, value) in recorded {
                let inner = format!("{path}.{key}");
                let Some(now) = current.get(key) else {
                    return Err(format!("{inner} is no longer sent"));
                };
                says_at_least(value, now, &inner)?;
            }
            Ok(())
        }
        (Value::Array(recorded), Value::Array(current)) => {
            if recorded.len() != current.len() {
                return Err(format!("{path} changed length"));
            }
            recorded
                .iter()
                .zip(current)
                .enumerate()
                .try_for_each(|(index, (recorded, current))| {
                    says_at_least(recorded, current, &format!("{path}[{index}]"))
                })
        }
        (recorded, current) if recorded == current => Ok(()),
        (recorded, current) => Err(format!("{path} was {recorded} and is now {current}")),
    }
}

#[test]
fn every_recorded_message_still_decodes_and_says_the_same() {
    let recordings = recordings();
    assert!(
        !recordings.is_empty(),
        "no recordings at {}",
        recordings_path().display()
    );
    for recording in recordings {
        let current = recording
            .direction
            .round_trip(&recording.message)
            .unwrap_or_else(|error| panic!("{} no longer decodes: {error}", recording.name));
        if let Err(difference) = says_at_least(&recording.message, &current, "") {
            panic!("{}: {difference}", recording.name);
        }
    }
}

#[test]
fn every_example_is_recorded() {
    let recorded: BTreeMap<String, Recording> = recordings()
        .into_iter()
        .map(|recording| (recording.name.clone(), recording))
        .collect();
    let mut missing = Vec::new();
    for (name, (direction, message)) in examples() {
        assert_eq!(
            direction.round_trip(&message).unwrap(),
            message,
            "{name} doesn't survive a round trip"
        );
        match recorded.get(name) {
            Some(recording) => {
                assert_eq!(recording.direction, direction, "{name}");
                if let Err(difference) = says_at_least(&recording.message, &message, "") {
                    panic!("{name}: {difference}");
                }
            }
            None => missing.push((name, direction, message)),
        }
    }
    if missing.is_empty() {
        return;
    }
    let lines: String = missing
        .iter()
        .map(|(name, direction, message)| {
            json!({"name": name, "direction": direction.name(), "message": message}).to_string()
                + "\n"
        })
        .collect();
    // The build host may run this test in a copy of the checkout, so the
    // lines are also printed for appending by hand.
    if std::env::var_os(RECORD).is_some() {
        let path = recordings_path();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&lines);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        println!("recorded in {}:\n{lines}", path.display());
        return;
    }
    panic!(
        "not recorded; append these lines to crates/mobile-api/fixtures/v1.jsonl, \
         or run with {RECORD}=1 in this checkout:\n{lines}"
    );
}

#[test]
fn a_reader_tolerates_what_a_later_version_adds() {
    let line = json!({
        "type": "event",
        "event": "sessions",
        "data": {
            "sessions": [{
                "id": "s", "lineage": "s", "surface": "hologram", "activity": "dreaming",
                "exited": false, "watchable": false, "mood": "fine"
            }],
            "page": 2
        }
    });
    let ServerLine::Event(Event::Sessions(sessions)) = serde_json::from_value(line).unwrap() else {
        panic!("a sessions event");
    };
    assert_eq!(sessions.sessions[0].surface, Surface::Unknown);
    assert_eq!(sessions.sessions[0].activity, Activity::Unknown);

    let item: Item = serde_json::from_value(json!({
        "id": "x", "role": "narrator", "kind": "hologram", "colour": "blue"
    }))
    .unwrap();
    assert_eq!(item.role, Role::Unknown);
    assert_eq!(item.body, ItemBody::Unknown);

    // A server reads a parameter a later client adds, even to an op that
    // takes none today.
    let line: ClientLine = serde_json::from_value(json!({
        "type": "request", "id": 1, "op": "sessions.watch", "params": {"include_closed": true}
    }))
    .unwrap();
    assert_eq!(
        line,
        ClientLine::Request(Request {
            id: 1,
            call: Call::WatchSessions {}
        })
    );
    let line: ClientLine = serde_json::from_value(json!({
        "type": "request", "id": 2, "op": "conversation.send",
        "params": {"session": "s", "text": "hi", "mode": "later", "attachments": []}
    }))
    .unwrap();
    assert!(matches!(
        line,
        ClientLine::Request(Request {
            call: Call::Send {
                mode: SendMode::Unknown,
                ..
            },
            ..
        })
    ));
}
