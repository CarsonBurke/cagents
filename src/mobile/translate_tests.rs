use std::sync::Arc;

use super::*;
use crate::{
    agent_execution::{Lineage, Origin},
    model::{AgentId, TerminalId},
};

fn text(id: &str, body: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: chat::ItemContent::Text {
            text: body.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: Some(1),
        at: Some(1),
    }
}

fn command(id: &str, output: &str) -> chat::Item {
    chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: chat::ItemContent::Command {
            label: "the tests".into(),
            command: "cargo test".into(),
            working_directory: "/home/person/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Running,
            exit_code: None,
            duration_ms: None,
            output: output.into(),
        },
        started_at: Some(1),
        at: Some(1),
    }
}

fn snapshot(items: Vec<chat::Item>) -> chat::Snapshot {
    chat::Snapshot {
        generation: 1,
        items: items.into_iter().map(Arc::new).collect(),
        ready: true,
        ..chat::Snapshot::default()
    }
}

#[test]
fn a_watch_starts_with_everything_and_then_streams_text_as_appends() {
    let mut mirror = Mirror::default();
    let first = mirror
        .update("s", &snapshot(vec![text("a", "Hello")]), true)
        .unwrap();
    assert!(first.reset);
    assert_eq!(first.order.as_deref(), Some(&["a".to_owned()][..]));
    assert_eq!(first.items.len(), 1);
    assert!(first.appends.is_empty());

    // An emoji is two UTF-16 code units, which is what a Kotlin string counts.
    let mut grown = text("a", "Hello 👋 there");
    grown.at = Some(2);
    let mut next = snapshot(vec![grown]);
    next.busy = true;
    let streamed = mirror.update("s", &next, false).unwrap();
    assert!(!streamed.reset);
    assert_eq!(streamed.order, None);
    assert!(streamed.items.is_empty());
    assert_eq!(
        streamed.appends,
        vec![api::TextAppend {
            id: "a".into(),
            offset: 5,
            text: " 👋 there".into(),
            at: Some(2),
        }]
    );
    assert!(streamed.state.busy);

    let mut more = text("a", "Hello 👋 there!");
    more.at = Some(3);
    let mut next = snapshot(vec![more]);
    next.busy = true;
    let streamed = mirror.update("s", &next, false).unwrap();
    assert_eq!(
        streamed.appends[0].offset,
        "Hello 👋 there".encode_utf16().count() as u64
    );
    assert_eq!(streamed.appends[0].text, "!");
}

#[test]
fn nothing_is_sent_when_nothing_the_phone_shows_changed() {
    let mut mirror = Mirror::default();
    let held = snapshot(vec![text("a", "Hello")]);
    mirror.update("s", &held, true).unwrap();
    let mut republished = held.clone();
    republished.generation = 2;
    assert_eq!(mirror.update("s", &republished, false), None);
}

#[test]
fn new_items_and_rewritten_ones_are_sent_whole_with_the_new_order() {
    let mut mirror = Mirror::default();
    mirror
        .update("s", &snapshot(vec![text("a", "Hello")]), true)
        .unwrap();
    let update = mirror
        .update(
            "s",
            &snapshot(vec![text("a", "Goodbye"), command("b", "running")]),
            false,
        )
        .unwrap();
    assert_eq!(
        update.order,
        Some(vec!["a".to_owned(), "b".to_owned()]),
        "a new item changes the order"
    );
    let ids: Vec<_> = update.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"], "a rewrite isn't an append");
    assert!(update.appends.is_empty());
}

#[test]
fn long_tool_output_keeps_its_end() {
    let output = format!("{}é\nok: 12 passed", "x".repeat(OUTPUT_TAIL_BYTES));
    let api::ItemBody::Command {
        output: sent,
        clipped,
        ..
    } = item(&command("b", &output)).body
    else {
        panic!("a command stays a command");
    };
    assert!(clipped);
    assert!(sent.ends_with("ok: 12 passed"));
    assert!(sent.len() <= OUTPUT_TAIL_BYTES);
}

#[test]
fn clipping_never_splits_a_character() {
    let text = "ééééé";
    assert_eq!(tail(text, 3), ("é", true));
    assert_eq!(head(text, 3), ("é", true));
    assert_eq!(head(text, 10), (text, false));
}

#[test]
fn a_file_change_counts_lines_without_the_patch_headers() {
    let change = chat::FileChange {
        path: "src/new.rs".into(),
        kind: chat::ChangeKind::Renamed {
            from: "src/old.rs".into(),
        },
        diff: "--- a/src/old.rs\n+++ b/src/new.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n+added\n same\n"
            .into(),
        lines: chat::LineOrigin::File,
    };
    assert_eq!(
        file_change(&change),
        api::FileChange {
            path: "src/new.rs".into(),
            change: api::ChangeKind::Renamed,
            from: Some("src/old.rs".into()),
            added_lines: 2,
            removed_lines: 1,
        }
    );
}

#[test]
fn a_pending_approval_reaches_the_phone_with_its_choices() {
    let mut held = snapshot(Vec::new());
    held.waiting = true;
    held.requests = vec![chat::Request {
        kind: chat::RequestKind::Approval,
        id: "r1".into(),
        title: "Run cargo test?".into(),
        description: "cargo test --all".into(),
        fields: Vec::new(),
        actions: vec![
            chat::Choice {
                id: "allow".into(),
                label: "Allow".into(),
                description: String::new(),
            },
            chat::Choice {
                id: "deny".into(),
                label: "Deny".into(),
                description: String::new(),
            },
        ],
    }];
    let state = conversation_state(&held);
    assert_eq!(state.activity, api::Activity::Waiting);
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[0].kind, api::RequestKind::Approval);
    let actions: Vec<_> = state.requests[0]
        .actions
        .iter()
        .map(|choice| choice.id.as_str())
        .collect();
    assert_eq!(actions, ["allow", "deny"]);
}

#[test]
fn a_delegated_agent_names_its_owner() {
    let id = TerminalId::new();
    let owner = Lineage(TerminalId::new());
    let agent = AgentId::new();
    let member = directory::Member {
        session: directory::Entry {
            id,
            surface: directory::Surface::Chat,
            provider: Some(Provider::Codex),
            title: Some("Fix the build".into()),
            channel: None,
            view_scope: None,
            channel_repository: None,
            lineage_channel: None,
            state: ActivityState::Working,
            directory: Some("/home/person/project".into()),
            worktree: None,
            project: None,
            project_name: Some("project".into()),
            delegation: Some(directory::Delegation {
                agent,
                owner: directory::Owner::from(&Origin::Child { parent: owner }),
                run: None,
                run_state: Some(State::Running),
            }),
            relation: directory::Relation::Peer,
            lineage: Lineage(id),
        },
        exited: false,
        failure: None,
        active_at: Some(42),
        limit: None,
        account: None,
        cache: None,
    };
    let session = session(&member);
    assert_eq!(session.id, id.to_string());
    assert_eq!(session.provider.as_deref(), Some("codex"));
    assert_eq!(session.activity, api::Activity::Working);
    assert!(session.watchable);
    assert_eq!(
        session.agent,
        Some(api::Agent {
            id: agent.to_string(),
            owner: api::AgentOwner::Session,
            owner_lineage: Some(owner.to_string()),
            run: Some(api::RunState::Running),
        })
    );
}

#[test]
fn only_unacknowledged_notices_reach_the_phone_newest_first() {
    let notice = |acknowledged, preview: &str| notifications::Notice {
        id: uuid::Uuid::new_v4(),
        event: notifications::Event {
            target: notifications::Target::Session(TerminalId::new()),
            source_session: None,
            provider: Some(Provider::Claude),
            directory: None,
            destination: None,
            evidence: notifications::Evidence {
                key: preview.into(),
                kind: notifications::Kind::Waiting,
            },
            content: Some(notifications::Content {
                conversation: "Fix the build".into(),
                preview: preview.into(),
                request: Some(notifications::RequestType::Approval),
            }),
        },
        delivered: true,
        acknowledged,
        window: None,
    };
    let snapshot = notifications::Snapshot {
        notices: vec![
            notice(false, "older"),
            notice(true, "dismissed"),
            notice(false, "newer"),
        ],
        diagnostic: None,
    };
    let sent = notices(&snapshot);
    let previews: Vec<_> = sent
        .iter()
        .map(|notice| notice.preview.as_deref().unwrap())
        .collect();
    assert_eq!(previews, ["newer", "older"]);
    assert_eq!(sent[0].request, Some(api::RequestKind::Approval));
    assert_eq!(sent[0].title, "Agent needs your attention");
    assert!(sent.iter().all(|notice| notice.asks));

    let mut finished = notice(false, "done");
    finished.event.evidence.kind = notifications::Kind::Succeeded;
    finished.event.content = None;
    assert!(!notices(&notifications::Snapshot {
        notices: vec![finished],
        diagnostic: None,
    })[0].asks);
}
