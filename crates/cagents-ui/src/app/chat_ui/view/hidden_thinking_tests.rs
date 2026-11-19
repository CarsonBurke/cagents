//! Reasoning with nothing to read, in the transcripts that draw it.

use super::figure_tests::{texts, view};
use super::*;
use iced::advanced::{Layout, layout, widget::Tree};
use iced::{Rectangle, Size};

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
            "Ran cargo test c1",
            "Thought for 17s",
            "Ran cargo test c2",
            "Ran cargo test c3"
        ],
        "only the long thought is listed, where it fell: {shown:?}"
    );
}

#[test]
fn a_subagent_that_has_only_thought_says_it_is_working() {
    let mut agent = chat::Subagent {
        items: vec![Arc::new(thought("child-thought", 1_000, 2_000))],
        status: chat::ToolStatus::Running,
        ..super::tests::native_child_fixture()
    };
    assert_eq!(
        child_placeholder(&agent),
        Some("Waiting for subagent activity…")
    );
    agent.status = chat::ToolStatus::Succeeded;
    assert_eq!(
        child_placeholder(&agent),
        Some("No recent subagent activity is available here.")
    );
    // Its first row replaces the placeholder.
    agent.status = chat::ToolStatus::Running;
    agent
        .items
        .push(Arc::new(command("child-command", 2_000, 3_000)));
    assert_eq!(child_placeholder(&agent), None);
    // A failure with its own detail says what happened instead.
    agent.items.truncate(1);
    agent.status = chat::ToolStatus::Failed;
    agent.detail = "The provider stopped.".into();
    assert_eq!(child_placeholder(&agent), None);
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
    let shown = texts(&runtime, &mut None);
    assert!(
        shown.iter().any(|text| text == "Ran 2 commands · working"),
        "{shown:?}"
    );
    assert_eq!(runtime.turns[0].summary, "Ran 2 commands · working");
    // The same items once the agent has stopped: only the busy state moved.
    publish(&mut runtime, false);
    assert!(!runtime.turns[0].running);
    // From the prompt to the end of the thought.
    assert_eq!(runtime.turns[0].working_for(60_000), Some(4_000));
}

#[test]
fn a_lone_command_followed_by_thinking_counts_on_as_its_group() {
    let (mut runtime, handle) = prepared_runtime_for_test(&[
        prompt(),
        command("c1", 1_000, 2_000),
        thought("t", 2_000, 3_000),
    ]);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    let shown = texts(&runtime, &mut None);
    assert!(
        shown
            .iter()
            .any(|text| text == "Ran cargo test c1 · working"),
        "the strip says the group is still working: {shown:?}"
    );
}

/// Each text the transcript shows, with where it is drawn, read from a tree
/// kept across frames.
fn placed_texts(runtime: &Runtime, tree: &mut Option<Tree>) -> Vec<(String, f32, f32)> {
    use iced::advanced::widget::Operation;
    struct Placed(Vec<(String, f32, f32)>);
    impl Operation for Placed {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds.y, bounds.height));
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = view(runtime);
    let tree = match tree {
        Some(tree) => {
            tree.diff(element.as_widget_mut());
            tree
        }
        None => tree.insert(Tree::new(&element)),
    };
    let size = Size::new(620.0, 720.0);
    let node =
        element
            .as_widget_mut()
            .layout(tree, &renderer, &layout::Limits::new(Size::ZERO, size));
    let mut placed = Placed(Vec::new());
    element
        .as_widget_mut()
        .operate(tree, Layout::new(&node), &renderer, &mut placed);
    placed.0
}

#[test]
fn a_lone_step_becomes_its_strip_and_back_without_moving_the_transcript() {
    let (mut runtime, handle) = prepared_runtime_for_test(&[prompt(), command("c1", 1_000, 2_000)]);
    let mut tree = None;
    let mut publish =
        |runtime: &mut Runtime, tree: &mut Option<Tree>, item: Option<chat::Item>, busy: bool| {
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.busy = busy;
            snapshot.items.extend(item.map(Arc::new));
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            preparation::drain_fixture(runtime);
            // The rows mount on the frame after the first.
            placed_texts(runtime, tree);
            placed_texts(runtime, tree)
        };
    // Where the group's line is, and the first line drawn below it.
    let rows = |shown: &[(String, f32, f32)]| {
        let at = shown
            .iter()
            .position(|(text, ..)| text.starts_with("Ran cargo test c1"))
            .unwrap_or_else(|| panic!("no group line: {shown:?}"));
        let (_, y, height) = shown[at];
        let below = shown[at + 1..]
            .iter()
            .find(|(_, other, _)| *other >= y + height)
            .map(|(_, other, _)| *other);
        (y, height, below)
    };
    let lone = publish(&mut runtime, &mut tree, None, true);
    let thinking = publish(
        &mut runtime,
        &mut tree,
        Some(thought("t", 2_000, 3_000)),
        true,
    );
    assert!(
        thinking
            .iter()
            .any(|(text, ..)| text == "Ran cargo test c1 · working"),
        "{thinking:?}"
    );
    let (lone_y, lone_height, lone_below) = rows(&lone);
    let (strip_y, strip_height, strip_below) = rows(&thinking);
    assert_eq!(
        (strip_y, strip_height),
        (lone_y, lone_height),
        "{lone:?}\n{thinking:?}"
    );
    assert_eq!(strip_below, lone_below, "{lone:?}\n{thinking:?}");
    // The answer ends the thinking: the row returns where the strip was.
    let answered = publish(
        &mut runtime,
        &mut tree,
        Some(stamped(
            "answer",
            chat::Role::Assistant,
            ItemContent::Text {
                text: "The tests pass.".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            3_000,
            4_000,
        )),
        false,
    );
    let (answered_y, answered_height, _) = rows(&answered);
    assert_eq!(
        (answered_y, answered_height),
        (lone_y, lone_height),
        "{answered:?}"
    );
}

#[test]
fn a_lone_steps_open_choice_survives_its_strip() {
    let (runtime, handle) = prepared_runtime_for_test(&[
        prompt(),
        command("c1", 1_000, 2_000),
        thought("t", 2_000, 3_000),
    ]);
    let target = runtime.target;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.cards
        .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
    // The reader opens the lone row.
    drop(app.handle_chat_ui(Action::Toggle(target, "c1".into())));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    handle.publish_delta_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert!(runtime.turns[0].thinking);
    assert!(
        runtime.group_open(&runtime.turns[0]),
        "the strip opens over it"
    );
    // Closing the strip closes the row it turns back into.
    let key = runtime.turns[0].key.clone();
    drop(app.handle_chat_ui(Action::ToggleTurn(target, key)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(!runtime.group_open(&runtime.turns[0]));
    assert!(!runtime.item_expanded("c1"));
}
