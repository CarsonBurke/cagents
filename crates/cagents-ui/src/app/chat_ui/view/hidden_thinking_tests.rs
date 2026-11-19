//! Reasoning with nothing to read, in the transcripts that draw it.

use super::figure_tests::texts;
use super::*;

fn stamped(
    id: &str,
    role: chat::Role,
    content: ItemContent,
    started_at: u64,
    at: u64,
) -> chat::Item {
    chat::Item {
        id: id.into(),
        role,
        content,
        started_at: Some(started_at),
        at: Some(at),
    }
}

fn command(id: &str, started_at: u64, at: u64) -> chat::Item {
    stamped(
        id,
        chat::Role::Assistant,
        ItemContent::Command {
            label: String::new(),
            command: format!("cargo test {id}"),
            working_directory: "/project".into(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: None,
            output: String::new(),
        },
        started_at,
        at,
    )
}

/// What Claude sends for most thinking: a signature and no text.
fn thought(id: &str, started_at: u64, at: u64) -> chat::Item {
    stamped(
        id,
        chat::Role::Assistant,
        ItemContent::Text {
            text: String::new(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at,
        at,
    )
}

fn prompt() -> chat::Item {
    stamped(
        "prompt",
        chat::Role::User,
        ItemContent::Text {
            text: "Run the tests".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        0,
        0,
    )
}

#[test]
fn an_open_group_says_how_long_it_spent_thinking_between_its_steps() {
    let (mut runtime, _handle) = prepared_runtime_for_test(&[
        prompt(),
        command("c1", 1_000, 2_000),
        thought("long", 2_000, 19_000),
        command("c2", 19_000, 20_000),
        thought("short", 20_000, 21_000),
        command("c3", 21_000, 22_000),
    ]);
    let mut tree = None;
    let shown = texts(&runtime, &mut tree);
    assert!(
        !shown.iter().any(|text| text.starts_with("Thought for")),
        "a closed group keeps its one line: {shown:?}"
    );
    let key = runtime.turns[0].key.clone();
    runtime.opened.edit().insert(key, true);
    let shown = texts(&runtime, &mut tree);
    let rows: Vec<_> = shown
        .iter()
        .filter(|text| text.starts_with("Thought for") || text.contains("cargo test"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        rows,
        [
            "cargo test c1",
            "Thought for 17s",
            "cargo test c2",
            "cargo test c3"
        ],
        "only the long thought is listed, where it fell: {shown:?}"
    );
}

#[test]
fn a_subagent_that_has_only_thought_says_it_is_working() {
    let (mut runtime, _handle) = prepared_runtime_for_test(&[prompt()]);
    let agent = chat::Subagent {
        id: "child".into(),
        items: vec![Arc::new(thought("child-thought", 1_000, 2_000))],
        status: chat::ToolStatus::Running,
        ..super::tests::native_child_fixture()
    };
    Arc::make_mut(&mut runtime.snapshot).subagents.push(agent);
    runtime.selected_subagent = Some("child".into());
    let snapshot = runtime.snapshot.clone();
    runtime.prepare_child_cache(&snapshot);
    preparation::drain_fixture(&mut runtime);
    let shown = texts(&runtime, &mut None);
    assert!(
        shown
            .iter()
            .any(|text| text == "Waiting for subagent activity…"),
        "{shown:?}"
    );
}

#[test]
fn a_group_counts_on_while_the_agent_thinks_and_stops_when_it_is_done() {
    let (mut runtime, handle) = prepared_runtime_for_test(&[
        prompt(),
        command("c1", 1_000, 2_000),
        command("c2", 2_000, 3_000),
        thought("t", 3_000, 4_000),
    ]);
    let publish = |runtime: &mut Runtime, busy: bool| {
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.busy = busy;
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        preparation::drain_fixture(runtime);
    };
    publish(&mut runtime, true);
    assert!(runtime.turns[0].running, "thinking is work still going on");
    assert_eq!(runtime.turns[0].summary, "Ran 2 commands · working");
    // The same items once the agent has stopped: only the busy state moved.
    publish(&mut runtime, false);
    assert!(!runtime.turns[0].running);
    assert_eq!(runtime.turns[0].working_for(60_000), Some(3_000));
}
