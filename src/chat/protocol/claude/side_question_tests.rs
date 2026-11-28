//! `/btw`: Claude Code's `side_question` control request, which answers from
//! the conversation's context without tools and records nothing.

use super::*;

fn connected(version: &str) -> Claude {
    let mut adapter = Claude {
        ready: true,
        ..Default::default()
    };
    adapter
        .receive(
            json!({"type":"system","subtype":"init","claude_code_version":version,
            "slash_commands":["btw","compact","review"]}),
        )
        .unwrap();
    adapter
}

fn question(id: &str, text: &str) -> chat::SideQuestion {
    chat::SideQuestion {
        id: id.into(),
        question: text.into(),
        history: Vec::new(),
    }
}

fn answers(step: &Step) -> Vec<&chat::SideAnswer> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::SideAnswer(answer) => Some(answer),
            _ => None,
        })
        .collect()
}

fn respond(adapter: &mut Claude, request: &Value, response: Value) -> Step {
    let mut response = response;
    response["request_id"] = request["request_id"].clone();
    adapter
        .receive(json!({"type":"control_response","response":response}))
        .unwrap()
}

#[test]
fn side_questions_follow_the_cli_version_that_answers_them() {
    let older = connected("2.1.267");
    assert!(!older.capabilities().side_questions);
    let current = connected("2.1.268");
    assert!(current.capabilities().side_questions);
    let mut older = older;
    let refusal = older
        .command(Command::SideQuestion(question("q", "what?")))
        .unwrap_err();
    assert!(refusal.contains("2.1.268"), "{refusal}");
}

#[test]
fn the_cli_btw_command_is_not_offered_as_a_prompt() {
    let adapter = connected("2.1.332");
    let names: Vec<_> = adapter.commands.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"btw"), "{names:?}");
    assert!(names.contains(&"review"), "{names:?}");
}

#[test]
fn a_side_question_is_a_control_request_beside_a_running_turn() {
    let mut adapter = connected("2.1.332");
    adapter.busy = true;
    let mut asked = question("q1", "which file was that?");
    asked.history.push(chat::SideExchange {
        question: "earlier".into(),
        answer: "before".into(),
    });
    let step = adapter.command(Command::SideQuestion(asked)).unwrap();
    assert_eq!(step.writes.len(), 1);
    let write = &step.writes[0];
    assert_eq!(write["type"], "control_request");
    assert_eq!(
        write["request"],
        json!({"subtype":"side_question","question":"which file was that?",
            "history":[{"question":"earlier","response":"before"}]})
    );
    // Asking changes nothing the conversation shows.
    assert!(step.events.is_empty());
    assert!(adapter.busy);
    assert!(adapter.queued.is_empty());

    let step = respond(
        &mut adapter,
        write,
        json!({"subtype":"success","response":{"response":"`src/main.rs`","synthetic":false}}),
    );
    assert_eq!(
        answers(&step),
        vec![&chat::SideAnswer::answered("q1", "`src/main.rs`")]
    );
    assert_eq!(step.events.len(), 1, "{:?}", step.events);
}

#[test]
fn a_side_question_without_history_sends_none() {
    let mut adapter = connected("2.1.332");
    let step = adapter
        .command(Command::SideQuestion(question("q", "why?")))
        .unwrap();
    assert!(step.writes[0]["request"].get("history").is_none());
    assert!(step.writes[0]["request"].get("_cagents_side").is_none());
}

#[test]
fn a_side_question_that_fails_tells_its_asker_not_the_conversation() {
    let mut adapter = connected("2.1.332");
    let ask = |adapter: &mut Claude, id: &str| {
        adapter
            .command(Command::SideQuestion(question(id, "why?")))
            .unwrap()
            .writes
            .remove(0)
    };
    let request = ask(&mut adapter, "error");
    let step = respond(
        &mut adapter,
        &request,
        json!({"subtype":"error","error":"Overloaded"}),
    );
    assert_eq!(
        answers(&step),
        vec![&chat::SideAnswer::failed(
            "error",
            "Claude couldn't answer: Overloaded"
        )]
    );
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "{:?}",
        step.events
    );

    let request = ask(&mut adapter, "null");
    let step = respond(
        &mut adapter,
        &request,
        json!({"subtype":"success","response":{"response":null}}),
    );
    assert!(matches!(
        &answers(&step)[0].outcome,
        chat::SideOutcome::Failed(_)
    ));

    // Claude Code writes an API error as a synthetic answer.
    let request = ask(&mut adapter, "synthetic");
    let step = respond(
        &mut adapter,
        &request,
        json!({"subtype":"success","response":{"response":"API Error: 529","synthetic":true}}),
    );
    assert_eq!(
        answers(&step),
        vec![&chat::SideAnswer::failed("synthetic", "API Error: 529")]
    );
}

#[test]
fn side_question_progress_is_not_narrated() {
    let mut adapter = connected("2.1.332");
    let step = adapter
        .receive(json!({"type":"system","subtype":"control_request_progress",
            "request_id":"r","status":"requesting"}))
        .unwrap();
    assert!(step.events.is_empty(), "{:?}", step.events);
}

#[test]
fn a_read_only_conversation_asks_nothing() {
    let mut adapter = connected("2.1.332");
    adapter.history_only = true;
    assert!(
        adapter
            .command(Command::SideQuestion(question("q", "why?")))
            .is_err()
    );
}
