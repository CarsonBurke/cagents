//! Explicit SDK input receipts. Installed Claude 2.1.289 stamps the primary
//! submit UUID on assistant/stream messages and final results. Absorbed-input
//! lists, result carrier UUIDs, lifecycle completion and proximity prove no fate.
use super::*;
use crate::agent::ActivityOutcome;

const MESSAGES: usize = 128;
const BYTES: usize = 64 * 1024;
#[derive(Clone)]
struct Message {
    native: String,
    session: String,
    input: Option<String>,
    aliases: Vec<String>,
}
struct Terminal {
    input: String,
    session: String,
    outcome: ActivityOutcome,
    at: u64,
}
#[derive(Default)]
pub(super) struct Receipts {
    messages: VecDeque<Message>,
    terminals: VecDeque<Terminal>,
}
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}
fn aliases(events: &[Event]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut bytes = 0usize;
    for event in events {
        let id = match event {
            Event::Item(item) if item.role == Role::Assistant => Some(item.id.as_str()),
            Event::TextDelta { id, role: Role::Assistant, .. }
            | Event::OutputDelta { id, .. } | Event::InputDelta { id, .. } => Some(id.as_str()),
            _ => None,
        };
        if let Some(id) = id && !id.is_empty() && id.len() <= crate::chat::turn_outcomes::MAX_TURN_ID_BYTES && !ids.iter().any(|known| known == id) {
            if ids.len() == 128 || bytes.saturating_add(id.len()) > BYTES { break; }
            bytes += id.len();
            ids.push(id.to_owned());
        }
    }
    ids
}
fn outcome(frame: &Value) -> Option<ActivityOutcome> {
    let reason = frame["terminal_reason"].as_str();
    if frame["is_error"].as_bool().is_none() { return None; }
    if matches!(reason, Some("aborted_streaming" | "aborted_tools")) {
        return Some(ActivityOutcome::Cancelled);
    }
    if frame["subtype"] == "success" && frame["is_error"].as_bool() == Some(false) && reason == Some("completed") {
        return Some(ActivityOutcome::Succeeded);
    }
    // A generic error may represent interruption or a delivery/session failure.
    // Preserve unknown until the provider reports an actual terminal cause.
    if frame["is_error"].as_bool() != Some(true) || !frame["subtype"].as_str().is_some_and(|kind| kind.starts_with("error_")) || !matches!(reason, Some("blocking_limit" | "rapid_refill_breaker" | "prompt_too_long" | "image_error" | "model_error" | "api_error" | "malformed_tool_use_exhausted" | "budget_exhausted" | "structured_output_retry_exhausted" | "tool_deferred_unavailable" | "turn_setup_failed" | "max_turns")) {
        return None;
    }
    let mut text = super::super::provider_error_text(&frame["errors"]);
    if text.is_empty() { text = format!("Claude ended this input: {}", reason?); }
    text.truncate(text.floor_char_boundary(crate::chat::turn_outcomes::MAX_TURN_REASON_BYTES));
    Some(ActivityOutcome::Failed { reason: text })
}
fn finish(identity: &TurnIdentity, outcome: ActivityOutcome, at: u64, step: &mut Step) {
    let update = TurnUpdate::Finish { identity: identity.clone(), outcome, finished_at: at };
    if update.validate().is_ok() { step.events.push(Event::TurnOutcome(update)); }
}
impl Receipts {
    fn bytes(&self) -> usize {
        self.messages.iter().map(|message| message.native.len() + message.session.len() + message.input.as_ref().map_or(0, String::len) + message.aliases.iter().map(String::len).sum::<usize>()).sum::<usize>()
            + self.terminals.iter().map(|terminal| terminal.input.len() + terminal.session.len() + terminal.outcome.reason().map_or(0, str::len)).sum::<usize>()
    }
    fn trim(&mut self) {
        while self.messages.len() > MESSAGES || self.bytes() > BYTES {
            if self.messages.pop_front().is_none() { self.terminals.pop_front(); }
        }
        while self.terminals.len() > PENDING || self.bytes() > BYTES { self.terminals.pop_front(); }
    }
    pub(super) fn discard(&mut self, input: &str) {
        self.messages.retain(|message| message.input.as_deref() != Some(input));
        self.terminals.retain(|terminal| terminal.input != input);
    }
    pub(super) fn accepted(&mut self, input: &str, identity: &TurnIdentity, step: &mut Step) {
        for message in &mut self.messages {
            if message.input.as_deref() == Some(input) && message.session == identity.session_id {
                emit_bind(identity, std::mem::take(&mut message.aliases), step);
            }
        }
        let mut index = 0;
        while index < self.terminals.len() {
            if self.terminals[index].input == input && self.terminals[index].session == identity.session_id {
                let terminal = self.terminals.remove(index).unwrap();
                finish(identity, terminal.outcome, terminal.at, step);
            } else { index += 1; }
        }
    }
    pub(super) fn observe(&mut self, frame: &Value, step: &mut Step, stream_message: Option<&str>, confirmed: Option<&str>, submitted: impl Fn(&str) -> bool, accepted: impl Fn(&str, &str) -> Option<TurnIdentity>) {
        let Some(session) = frame["session_id"].as_str().filter(|session| Some(*session) == confirmed) else { return; };
        if !frame["parent_tool_use_id"].is_null() { return; }
        let primary = frame["user_message_uuid"].as_str().filter(|id| crate::chat::protocol::input_proof::valid_id(id) && submitted(id));
        if frame["type"] == "result" {
            let Some(input) = primary else { return; };
            let Some(outcome) = outcome(frame) else { return; };
            if let Some(identity) = accepted(input, session) { finish(&identity, outcome, now(), step); }
            else if !self.terminals.iter().any(|terminal| terminal.input == input && terminal.session == session) {
                self.terminals.push_back(Terminal { input: input.into(), session: session.into(), outcome, at: now() });
                self.trim();
            }
            return;
        }
        let native = match frame["type"].as_str() {
            Some("assistant") => frame["message"]["id"].as_str(),
            Some("stream_event") => stream_message,
            _ => return,
        }.filter(|id| crate::chat::protocol::input_proof::valid_id(id));
        let Some(native) = native else { return; };
        if frame.get("user_message_uuid").is_some() && primary.is_none() {
            // An explicit unknown/invalid owner is contradictory evidence,
            // never an unstamped continuation of an earlier owner.
            if let Some(message) = self.messages.iter_mut().find(|message| message.native == native && message.session == session) {
                message.input = None;
                message.aliases.clear();
            }
            return;
        }
        if let Some(input) = primary {
            if let Some(message) = self.messages.iter_mut().find(|message| message.native == native && message.session == session) {
                if message.input.as_deref() != Some(input) { message.input = None; message.aliases.clear(); }
            } else { self.messages.push_back(Message { native: native.into(), session: session.into(), input: Some(input.into()), aliases: vec![] }); }
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.native == native && message.session == session) else { return; };
        let Some(input) = message.input.as_deref() else { return; };
        let ids = aliases(&step.events);
        if let Some(identity) = accepted(input, session) { emit_bind(&identity, ids, step); }
        else {
            for id in ids { if !message.aliases.contains(&id) { message.aliases.push(id); } }
        }
        self.trim();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "00000000-0000-4000-8000-000000000001";
    fn adapter() -> Claude { Claude { ready: true, async_input: true, ..Claude::default() } }
    fn submit(adapter: &mut Claude) -> String {
        adapter.command(Command::Prompt { text: "identical user input".into(), attachments: vec![], mode: if adapter.busy { PromptMode::Queue } else { PromptMode::Send } }).unwrap().writes[0]["uuid"].as_str().unwrap().to_owned()
    }
    fn echo(input: &str) -> Value { json!({"type":"user","uuid":input,"session_id":SESSION,"message":{"id":format!("user-{input}"),"role":"user","content":"accepted"}}) }
    fn started(input: &str) -> Value { json!({"type":"command_lifecycle","command_uuid":input,"state":"started","session_id":SESSION}) }
    fn accept(adapter: &mut Claude, input: &str) {
        adapter.receive(echo(input)).unwrap(); adapter.receive(started(input)).unwrap();
    }
    fn result(input: &str, reason: &str) -> Value {
        json!({"type":"result","uuid":"distinct-result-carrier","session_id":SESSION,"user_message_uuid":input,
            "subtype":if reason=="completed" {"success"} else {"error_during_execution"},"is_error":reason!="completed","terminal_reason":reason,"errors":["Actual native error detail"]})
    }
    fn updates(step: &Step) -> Vec<&TurnUpdate> { step.events.iter().filter_map(|event| match event { Event::TurnOutcome(update) => Some(update), _ => None }).collect() }
    #[test]
    fn actual_decoder_primary_result_finishes_exact_input_after_a_newer_input_starts() {
        for reason in ["completed", "model_error", "aborted_streaming", "aborted_tools"] {
            let mut adapter = adapter(); let first = submit(&mut adapter); accept(&mut adapter, &first);
            let second = submit(&mut adapter); accept(&mut adapter, &second);
            let step = adapter.receive(result(&first, reason)).unwrap();
            let actual = updates(&step); assert_eq!(actual.len(), 1);
            let TurnUpdate::Finish { identity, outcome, .. } = actual[0] else { panic!("exact native finish required"); };
            assert_eq!(identity.id, first); assert_ne!(identity.id, second); assert_eq!(identity.session_id, SESSION);
            match reason { "completed" => assert_eq!(outcome, &ActivityOutcome::Succeeded), "model_error" => assert!(matches!(outcome, ActivityOutcome::Failed { reason } if reason == "Actual native error detail")), _ => assert_eq!(outcome, &ActivityOutcome::Cancelled) }
        }
    }
    #[test]
    fn explicit_result_before_delayed_acceptance_waits_for_exact_user_and_lifecycle_join() {
        let mut adapter = adapter(); let input = submit(&mut adapter);
        assert!(updates(&adapter.receive(result(&input, "aborted_tools")).unwrap()).is_empty());
        assert!(updates(&adapter.receive(echo(&input)).unwrap()).is_empty());
        let step = adapter.receive(started(&input)).unwrap();
        assert!(updates(&step).iter().any(|update| matches!(update, TurnUpdate::Finish { identity, outcome: ActivityOutcome::Cancelled, .. } if identity.id == input)));
    }
    #[test]
    fn missing_primary_ambiguous_cause_absorbed_list_and_unsubmitted_receipts_stay_unknown() {
        let mut adapter = adapter(); let input = submit(&mut adapter); accept(&mut adapter, &input);
        let mut cases = vec![result("unsubmitted", "completed"), result(&input, "unrecognized_cause"), result(&input, "tool_deferred")];
        let mut absent = result(&input, "completed"); absent.as_object_mut().unwrap().remove("user_message_uuid"); absent["user_message_uuids"] = json!([input]); cases.push(absent);
        let mut generic = result(&input, "model_error"); generic.as_object_mut().unwrap().remove("terminal_reason"); cases.push(generic);
        for frame in cases { assert!(updates(&adapter.receive(frame).unwrap()).is_empty()); }
        let mut wrong = result(&input, "completed"); wrong["session_id"] = json!("different-native-session"); assert!(adapter.receive(wrong).is_err());
    }
    #[test]
    fn exact_stamped_message_binds_only_its_own_tools_and_explicit_same_message_replay() {
        let mut adapter = adapter(); let input = submit(&mut adapter); accept(&mut adapter, &input);
        let assistant = |message: &str, tool: &str, stamped: bool| {
            let mut frame = json!({"type":"assistant","uuid":format!("carrier-{message}"),"session_id":SESSION,"message":{"id":message,"role":"assistant","content":[{"type":"tool_use","id":tool,"name":"Bash","input":{"command":"git status"}}]}});
            if stamped { frame["user_message_uuid"] = json!(input); } frame
        };
        let step = adapter.receive(assistant("native-message", "owned-tool", true)).unwrap();
        assert!(updates(&step).iter().any(|update| matches!(update, TurnUpdate::Bind { identity, item_ids } if identity.id == input && item_ids.contains(&"owned-tool".to_owned()))));
        let step = adapter.receive(assistant("unrelated-message", "unowned-tool", false)).unwrap(); assert!(updates(&step).is_empty());
        let step = adapter.receive(assistant("native-message", "same-message-tool", false)).unwrap();
        assert!(updates(&step).iter().any(|update| matches!(update, TurnUpdate::Bind { item_ids, .. } if item_ids.contains(&"same-message-tool".to_owned()))));
    }
    #[test]
    fn explicit_unknown_or_invalid_stamp_cannot_borrow_an_existing_message_owner() {
        for stamp in [json!("unsubmitted"), json!(""), Value::Null, json!(false)] {
            let mut adapter = adapter(); let input = submit(&mut adapter); accept(&mut adapter, &input);
            let mut assistant = json!({"type":"assistant","uuid":"carrier","session_id":SESSION,"user_message_uuid":input,"message":{"id":"same-native-message","role":"assistant","content":[{"type":"tool_use","id":"original-tool","name":"Bash","input":{}}]}});
            let first = adapter.receive(assistant.clone()).unwrap(); assert!(!updates(&first).is_empty());
            assistant["user_message_uuid"] = stamp;
            assistant["message"]["content"][0]["id"] = json!("contradictory-tool");
            assert!(updates(&adapter.receive(assistant.clone()).unwrap()).is_empty());
            assistant.as_object_mut().unwrap().remove("user_message_uuid");
            assistant["message"]["content"][0]["id"] = json!("after-conflict-tool");
            assert!(updates(&adapter.receive(assistant).unwrap()).is_empty(), "conflicting message proof must stay unqualified");
        }
    }

    #[test]
    fn stamped_stream_start_proves_only_that_native_messages_continuations() {
        let mut adapter = adapter(); let input = submit(&mut adapter); accept(&mut adapter, &input);
        let start = |id: &str, stamp: bool| { let mut frame = json!({"type":"stream_event","session_id":SESSION,"event":{"type":"message_start","message":{"id":id}}}); if stamp { frame["user_message_uuid"] = json!(input); } frame };
        adapter.receive(start("stream-owned", true)).unwrap();
        let block = |id: &str| json!({"type":"stream_event","session_id":SESSION,"event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":"Bash","input":{}}}});
        let step = adapter.receive(block("stream-owned-tool")).unwrap();
        assert!(updates(&step).iter().any(|update| matches!(update, TurnUpdate::Bind { item_ids, .. } if item_ids.contains(&"stream-owned-tool".to_owned()))));
        adapter.receive(start("stream-unowned", false)).unwrap();
        assert!(updates(&adapter.receive(block("stream-unowned-tool")).unwrap()).is_empty());
    }
    #[test]
    fn discarded_local_input_cannot_gain_fate_or_message_membership_from_late_receipts() {
        let mut adapter = adapter(); let input = submit(&mut adapter); adapter.turn_inputs.discard_undispatched(&input);
        adapter.receive(echo(&input)).unwrap(); adapter.receive(started(&input)).unwrap();
        assert!(updates(&adapter.receive(result(&input, "aborted_tools")).unwrap()).is_empty());
    }
}
