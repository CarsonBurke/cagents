//! Send now: handing what is queued to a running Claude turn without stopping
//! its subagents, as `interrupt_send_now_v1` offers.

use super::*;

/// A ready adapter for a CLI that advertises Send now, with a turn running.
fn working() -> Claude {
    let mut adapter = Claude {
        ready: true,
        ..Default::default()
    };
    adapter
        .receive(
            json!({"type":"system","subtype":"init","claude_code_version":"2.1.325",
            "capabilities":["interrupt_send_now_v1"]}),
        )
        .unwrap();
    adapter.busy = true;
    adapter
}

/// Queues `text` as `mode` and returns its ID, admitted by the CLI when
/// `admitted` says so.
fn queue(adapter: &mut Claude, text: &str, mode: PromptMode, admitted: bool) -> String {
    let step = adapter.send_prompt(text.into(), vec![], mode).unwrap();
    let id = string(&step.writes[0], "uuid").to_owned();
    if admitted {
        adapter
            .receive(json!({"type":"command_lifecycle","command_uuid":id,"state":"queued"}))
            .unwrap();
    }
    id
}

fn states(step: &Step) -> Vec<Option<chat::SendNow>> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::SendNow(state) => Some(*state),
            _ => None,
        })
        .collect()
}

fn requests(step: &Step) -> Vec<&Value> {
    step.writes
        .iter()
        .filter(|write| write["request"]["send_now"] == true)
        .collect()
}

fn errors(step: &Step) -> Vec<&str> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

/// Answers the request `step` wrote with `response`.
fn answer(adapter: &mut Claude, step: &Step, response: Value) -> Step {
    let id = string(requests(step)[0], "request_id").to_owned();
    adapter
        .receive(
            json!({"type":"control_response","response":{"subtype":"success",
            "request_id":id,"response":response}}),
        )
        .unwrap()
}

#[test]
fn only_a_cli_whose_init_advertises_it_offers_send_now() {
    let offered = |init: Value| {
        let mut adapter = Claude {
            ready: true,
            ..Default::default()
        };
        let step = adapter.receive(init).unwrap();
        let capabilities = step
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                Event::Capabilities(capabilities) => Some(capabilities.send_queued_now),
                _ => None,
            })
            .unwrap();
        assert_eq!(capabilities, adapter.capabilities().send_queued_now);
        capabilities
    };
    assert!(offered(
        json!({"type":"system","subtype":"init","claude_code_version":"2.1.325",
            "capabilities":["interrupt_send_now_v1"]})
    ));
    assert!(!offered(
        json!({"type":"system","subtype":"init","claude_code_version":"2.1.325"})
    ));
    assert!(!offered(
        json!({"type":"system","subtype":"init","claude_code_version":"2.1.325",
            "capabilities":["something_else"]})
    ));
    // Without the verified async input there is no queue to send.
    assert!(!offered(
        json!({"type":"system","subtype":"init","claude_code_version":"2.1.285",
            "capabilities":["interrupt_send_now_v1"]})
    ));

    let mut adapter = Claude {
        ready: true,
        busy: true,
        async_input: true,
        ..Default::default()
    };
    queue(&mut adapter, "now", PromptMode::Steer, true);
    // Any other CLI would read the request as a plain interrupt, which stops
    // background agents with the turn.
    assert!(adapter.command(Command::SendQueuedNow).is_err());
}

#[test]
fn the_request_names_the_first_queued_message_once_every_one_is_admitted() {
    let mut adapter = working();
    let later = queue(&mut adapter, "after the turn", PromptMode::Queue, true);
    let next = queue(&mut adapter, "at the next step", PromptMode::Steer, false);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    // A message Claude Code is still admitting would not be found waiting.
    assert_eq!(states(&asked), [Some(chat::SendNow::Requested)]);
    assert!(asked.writes.is_empty());
    // Asking again while it is held changes nothing.
    let again = adapter.command(Command::SendQueuedNow).unwrap();
    assert!(again.writes.is_empty() && again.events.is_empty());

    let admitted = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":next,"state":"queued"}))
        .unwrap();
    let written = requests(&admitted);
    assert_eq!(written.len(), 1);
    assert_eq!(
        written[0]["request"],
        json!({"subtype":"interrupt","send_now":true,"message_uuid":next})
    );
    assert_eq!(
        adapter.send_now_request.as_ref().unwrap().messages,
        [next, later]
    );
    let repeated = adapter.command(Command::SendQueuedNow).unwrap();
    assert!(repeated.writes.is_empty());
}

#[test]
fn delivering_lasts_until_the_messages_it_was_for_have_left_the_queue() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    let answered = answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"delivering"}),
    );
    assert_eq!(states(&answered), [Some(chat::SendNow::Delivering)]);
    assert!(errors(&answered).is_empty());
    // A message queued after the request waits its own turn, and does not
    // keep the request going.
    queue(&mut adapter, "second", PromptMode::Steer, true);
    let started = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":first,"state":"started"}))
        .unwrap();
    assert_eq!(states(&started), [None]);
    assert_eq!(adapter.queued.len(), 1);
    assert!(adapter.send_now_request.is_none());
    // It can then be sent now in its own right.
    let again = adapter.command(Command::SendQueuedNow).unwrap();
    assert_eq!(requests(&again).len(), 1);
}

#[test]
fn nothing_waiting_is_said_until_the_queue_changes() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Queue, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    let declined = answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[],"send_now":"nothing_waiting"}),
    );
    assert_eq!(states(&declined), [Some(chat::SendNow::Declined)]);
    assert!(errors(&declined).is_empty());
    // The person may ask again; it is a new request.
    let again = adapter.command(Command::SendQueuedNow).unwrap();
    assert_eq!(states(&again), [Some(chat::SendNow::Requested)]);
    answer(
        &mut adapter,
        &again,
        json!({"still_queued":[],"send_now":"nothing_waiting"}),
    );
    // What it answered for no longer waits as it did.
    queue(&mut adapter, "second", PromptMode::Queue, false);
    assert_eq!(adapter.send_now, Some(chat::SendNow::Declined));
    let changed = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":first,"state":"cancelled"}))
        .unwrap();
    assert_eq!(states(&changed), [None]);
}

#[test]
fn an_answer_that_says_nothing_of_send_now_or_fails_shows_nothing() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    let unknown = answer(&mut adapter, &asked, json!({"still_queued":[first]}));
    assert_eq!(states(&unknown), [None]);
    assert!(adapter.send_now_request.is_none());

    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    let id = string(requests(&asked)[0], "request_id").to_owned();
    let failed = adapter
        .receive(
            json!({"type":"control_response","response":{"subtype":"error",
            "request_id":id,"error":"not now"}}),
        )
        .unwrap();
    assert_eq!(states(&failed), [None]);
    assert_eq!(errors(&failed), ["Claude send now: not now"]);
}

#[test]
fn a_turn_cut_short_to_deliver_is_not_a_failure_or_a_stop() {
    let mut adapter = working();
    adapter.execution = Some(crate::model::RunId::new());
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"stopped"}),
    );
    assert_eq!(adapter.send_now, Some(chat::SendNow::Stopped));
    let ended = adapter
        .receive(
            json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "terminal_reason":"aborted_streaming","stop_reason":null,
            "errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}),
        )
        .unwrap();
    assert!(errors(&ended).is_empty(), "{:?}", errors(&ended));
    assert_eq!(states(&ended), [None]);
    // The turn it made way for carries the run on and reports for it.
    assert!(!ended.events.iter().any(|event| matches!(
        event,
        Event::TurnFinished { .. }
            | Event::Execution {
                event: crate::agent_execution::ProviderEvent::Settled
                    | crate::agent_execution::ProviderEvent::Finished { .. },
                ..
            }
    )));
    assert!(adapter.execution.is_some());
    assert!(!adapter.ended_interrupted);
    assert!(ended.events.iter().all(
        |event| !matches!(event, Event::Status { detail, .. } if detail == chat::TURN_INTERRUPTED)
    ));
}

fn finished(step: &Step) -> bool {
    step.events.iter().any(|event| {
        matches!(
            event,
            Event::TurnFinished {
                outcome: crate::notifications::Kind::Succeeded,
                ..
            }
        )
    })
}

/// A turn cut short for its queue stays at work until the messages start,
/// so the conversation never reads idle in between, and the turn they start
/// takes the run on.
#[test]
fn the_conversation_stays_at_work_until_what_it_made_way_for_starts() {
    let mut adapter = working();
    adapter.execution = Some(crate::model::RunId::new());
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"delivering"}),
    );
    let ended = adapter
        .receive(json!({"type":"result","subtype":"success","is_error":false,
            "terminal_reason":"aborted_streaming","stop_reason":"tool_use"}))
        .unwrap();
    assert!(adapter.busy);
    assert!(
        !ended
            .events
            .iter()
            .any(|event| matches!(event, Event::Waiting(false)))
    );
    let started = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":first,"state":"started"}))
        .unwrap();
    assert!(
        started
            .events
            .iter()
            .any(|event| matches!(event, Event::TurnStarted))
    );
    assert!(!finished(&started));
    assert!(adapter.hand_over.is_none());
    assert!(adapter.execution.is_some(), "the new turn carries the run");
}

/// Should what it made way for be taken back before it starts, the turn
/// that made way settles itself and its run, as a turn that finished.
#[test]
fn a_turn_whose_queue_is_taken_back_before_it_starts_settles_then() {
    let mut adapter = working();
    adapter.execution = Some(crate::model::RunId::new());
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"delivering"}),
    );
    let ended = adapter
        .receive(json!({"type":"result","subtype":"success","is_error":false,
            "terminal_reason":"aborted_streaming","stop_reason":"tool_use"}))
        .unwrap();
    assert!(!finished(&ended));
    let gone = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":first,"state":"cancelled"}))
        .unwrap();
    assert!(finished(&gone));
    assert!(gone.events.iter().any(|event| matches!(
        event,
        Event::Execution {
            event: crate::agent_execution::ProviderEvent::Finished { .. },
            ..
        }
    )));
    assert!(!adapter.busy);
    assert!(adapter.execution.is_none());
}

/// A message on its way back to the composer is neither named nor waited
/// for: the request names the next.
#[test]
fn a_message_being_taken_back_is_not_sent_now() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let second = queue(&mut adapter, "second", PromptMode::Steer, true);
    adapter
        .command(Command::RemoveQueued { id: first.clone() })
        .unwrap();
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    assert_eq!(
        requests(&asked)[0]["request"]["message_uuid"],
        second.as_str()
    );
    assert_eq!(
        adapter.send_now_request.as_ref().unwrap().messages,
        [second.clone()]
    );
    // Its leaving does not end the request for the one still waiting.
    let gone = adapter
        .receive(json!({"type":"command_lifecycle","command_uuid":first,"state":"cancelled"}))
        .unwrap();
    assert!(states(&gone).is_empty());
    assert_eq!(adapter.send_now, Some(chat::SendNow::Requested));
}

/// Claude Code's accounts of a turn it cut short, and of turns that ended on
/// their own, which must not be taken for making way.
#[test]
fn only_a_result_that_names_an_abort_or_stops_for_a_call_is_cut_short() {
    for frame in [
        json!({"subtype":"success","stop_reason":"tool_use","terminal_reason":"aborted_streaming"}),
        json!({"subtype":"success","stop_reason":null,"terminal_reason":"aborted_streaming"}),
        json!({"subtype":"success","stop_reason":"tool_use"}),
        json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools",
            "errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"]}),
    ] {
        assert!(cut_short(&frame), "{frame}");
    }
    for frame in [
        json!({"subtype":"success","stop_reason":"end_turn","terminal_reason":"completed"}),
        json!({"subtype":"success","stop_reason":"end_turn"}),
        json!({"subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming",
            "errors":["API Error: 529 overloaded"]}),
        json!({"subtype":"error_max_turns","is_error":true}),
        json!({"subtype":"error_during_execution","is_error":true}),
        json!({"subtype":"error_during_execution","is_error":true,"errors":[]}),
    ] {
        assert!(!cut_short(&frame), "{frame}");
    }
}

#[test]
fn a_stop_pressed_meanwhile_is_still_the_persons_stop() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"delivering"}),
    );
    let stop = adapter.command(Command::Interrupt).unwrap();
    let id = string(&stop.writes[0], "request_id").to_owned();
    adapter
        .receive(
            json!({"type":"control_response","response":{"subtype":"success",
            "request_id":id,"response":{"still_queued":[first]}}}),
        )
        .unwrap();
    let ended = adapter
        .receive(
            json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "terminal_reason":"aborted_tools","stop_reason":"tool_use",
            "errors":["[ede_diagnostic] result_type=user"]}),
        )
        .unwrap();
    assert_eq!(states(&ended), [None]);
    assert!(adapter.ended_interrupted);
}

#[test]
fn a_real_failure_during_send_now_is_still_reported() {
    let mut adapter = working();
    let first = queue(&mut adapter, "first", PromptMode::Steer, true);
    let asked = adapter.command(Command::SendQueuedNow).unwrap();
    answer(
        &mut adapter,
        &asked,
        json!({"still_queued":[first],"send_now":"delivering"}),
    );
    let ended = adapter
        .receive(
            json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "errors":["API Error: 529 overloaded"]}),
        )
        .unwrap();
    assert_eq!(errors(&ended), ["API Error: 529 overloaded"]);
    assert!(ended.events.iter().any(|event| matches!(
        event,
        Event::TurnFinished {
            outcome: crate::notifications::Kind::Failed,
            ..
        }
    )));
}

#[test]
fn a_reset_forgets_the_request() {
    let mut adapter = working();
    queue(&mut adapter, "first", PromptMode::Steer, true);
    adapter.command(Command::SendQueuedNow).unwrap();
    adapter
        .receive(json!({"type":"conversation_reset"}))
        .unwrap();
    assert!(adapter.send_now.is_none() && adapter.send_now_request.is_none());
}

/// Replays of what Claude Code 2.1.296 did, recorded by the #2061 harness:
/// the frames the CLI wrote, and the requests the recorder made, which are
/// made again through the adapter here. IDs the adapter chooses afresh are
/// translated in the CLI's later frames.
mod recorded {
    use super::*;

    struct Replay {
        adapter: Claude,
        events: Vec<Event>,
        writes: Vec<Value>,
        names: HashMap<String, String>,
    }

    fn translate(value: &mut Value, names: &HashMap<String, String>) {
        match value {
            Value::String(text) => {
                if let Some(name) = names.get(text.as_str()) {
                    *text = name.clone();
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|v| translate(v, names)),
            Value::Object(values) => values.values_mut().for_each(|v| translate(v, names)),
            _ => {}
        }
    }

    impl Replay {
        fn take(&mut self, step: Step) -> Vec<Value> {
            self.events.extend(step.events);
            self.writes.extend(step.writes.iter().cloned());
            step.writes
        }

        fn asked(&mut self, frame: &Value) {
            let (recorded, step) = if frame["type"] == "user" {
                let mode = match frame["priority"].as_str() {
                    Some("next") => PromptMode::Steer,
                    Some("later") => PromptMode::Queue,
                    _ => PromptMode::Send,
                };
                let text = frame["message"]["content"][0]["text"].as_str().unwrap();
                let step = self.adapter.send_prompt(text.into(), vec![], mode).unwrap();
                (string(frame, "uuid").to_owned(), step)
            } else {
                let command = if frame["request"]["send_now"] == true {
                    Command::SendQueuedNow
                } else {
                    Command::Interrupt
                };
                let step = self.adapter.command(command).unwrap();
                (string(frame, "request_id").to_owned(), step)
            };
            let writes = self.take(step);
            if let Some(chosen) = writes
                .iter()
                .find_map(|write| write["uuid"].as_str().or(write["request_id"].as_str()))
            {
                self.names.insert(recorded, chosen.to_owned());
            }
        }

        fn play(name: &str, body: &str) -> Self {
            let mut replay = Self {
                adapter: Claude {
                    ready: true,
                    ..Default::default()
                },
                events: Vec::new(),
                writes: Vec::new(),
                names: HashMap::new(),
            };
            // The recorder allowed every permission request as soon as it was
            // asked, and kept no record of answering. Only a request Claude
            // itself withdrew went unanswered.
            let withdrawn: HashSet<String> = body
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record["frame"]["type"] == "control_cancel_request")
                .map(|record| string(&record["frame"], "request_id").to_owned())
                .collect();
            for line in body.lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                let mut frame = record["frame"].clone();
                match record["dir"].as_str() {
                    Some("in") => replay.asked(&frame),
                    // The recordings keep no tool input fragments, so a tool
                    // block streamed from them would close empty over the
                    // call its assistant frame delivers whole.
                    Some("out") if frame["event"]["content_block"]["type"] == "tool_use" => {}
                    Some("out") => {
                        translate(&mut frame, &replay.names);
                        let asked = (frame["type"] == "control_request"
                            && frame["request"]["subtype"] == "can_use_tool")
                            .then(|| string(&frame, "request_id").to_owned())
                            .filter(|id| !withdrawn.contains(id));
                        let step = replay
                            .adapter
                            .receive(frame)
                            .unwrap_or_else(|error| panic!("{name}: {error}\n{line}"));
                        replay.take(step);
                        if let Some(request_id) = asked {
                            let step = replay
                                .adapter
                                .command(Command::Answer {
                                    request_id,
                                    action: "allow".into(),
                                    answers: vec![],
                                })
                                .unwrap_or_else(|error| panic!("{name}: {error}\n{line}"));
                            replay.take(step);
                        }
                    }
                    _ => {}
                }
            }
            replay
        }

        fn errors(&self) -> Vec<&str> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    Event::Error { message, .. } => Some(message.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn outcomes(&self) -> Vec<crate::notifications::Kind> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    Event::TurnFinished { outcome, .. } => Some(*outcome),
                    _ => None,
                })
                .collect()
        }

        fn states(&self) -> Vec<Option<chat::SendNow>> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    Event::SendNow(state) => Some(*state),
                    _ => None,
                })
                .collect()
        }

        fn shown_text(&self) -> Vec<String> {
            self.events
                .iter()
                .filter_map(|event| match event {
                    Event::Item(Item {
                        content: ItemContent::Text { text, .. },
                        ..
                    }) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    macro_rules! trace {
        ($name:literal) => {
            Replay::play(
                $name,
                include_str!(concat!(
                    "../../../../tests/fixtures/claude-send-now/",
                    $name,
                    ".jsonl"
                )),
            )
        };
    }

    #[test]
    fn a_blocked_command_moves_aside_and_the_turn_takes_the_message() {
        let replay = trace!("sn_fg_bash_next");
        assert_eq!(
            replay.states(),
            [
                Some(chat::SendNow::Requested),
                Some(chat::SendNow::Delivering),
                None
            ]
        );
        let request = replay
            .writes
            .iter()
            .find(|write| write["request"]["send_now"] == true)
            .unwrap();
        assert!(request["request"].get("_cagents_turn").is_none());
        assert!(replay.errors().is_empty(), "{:?}", replay.errors());
        assert!(replay.adapter.queued.is_empty());
        assert!(
            replay
                .shown_text()
                .iter()
                .any(|text| text.starts_with("BANANA"))
        );
        assert!(
            !replay
                .outcomes()
                .contains(&crate::notifications::Kind::Cancelled)
        );
    }

    #[test]
    fn a_reply_stopped_to_deliver_is_neither_a_failure_nor_a_stop() {
        let replay = trace!("k_text_next");
        assert_eq!(
            replay.states(),
            [
                Some(chat::SendNow::Requested),
                Some(chat::SendNow::Stopped),
                None
            ]
        );
        assert!(replay.errors().is_empty(), "{:?}", replay.errors());
        // Only the turn it made way for reports, and it succeeded.
        assert_eq!(replay.outcomes(), [crate::notifications::Kind::Succeeded]);
        assert!(
            replay
                .shown_text()
                .iter()
                .any(|text| text.starts_with("BANANA"))
        );
    }

    #[test]
    fn a_turn_cut_short_to_deliver_a_message_for_its_end_reports_once() {
        let replay = trace!("sn_fg_bash_later");
        assert!(replay.states().contains(&Some(chat::SendNow::Delivering)));
        assert!(replay.errors().is_empty(), "{:?}", replay.errors());
        assert_eq!(replay.outcomes(), [crate::notifications::Kind::Succeeded]);
        assert!(replay.adapter.queued.is_empty());
    }

    #[test]
    fn a_stop_after_send_now_stops_the_turn_as_the_person_asked() {
        let replay = trace!("sn_interrupt_before");
        assert!(replay.errors().is_empty(), "{:?}", replay.errors());
        assert_eq!(
            replay.outcomes(),
            [
                crate::notifications::Kind::Cancelled,
                crate::notifications::Kind::Succeeded
            ]
        );
    }
}
