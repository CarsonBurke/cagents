//! Exact provider acceptance joins for immutable original commands.
//!
//! Call only for live provider frames. Expected session IDs are selectors,
//! never confirmation. Display aliases come from the actual item parser.
//! These helpers do not dispatch, persist, or infer acceptance from local runs.
//!
//! Claude SDK 0.3.236 system/user types and the official 0.2.72 changelog
//! document provider-resolved getSettings().applied model/effort values:
//! https://github.com/anthropics/claude-agent-sdk-typescript/blob/main/CHANGELOG.md
//! Codex contracts come from the installed 0.159.3 app-server v2 schema:
//! TurnStartParams.clientUserMessageId, UserMessage.clientId and TurnStartResponse.

use crate::chat::annotations::{OriginalError, OriginalSettings, Setting};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

const MAX_PENDING: usize = 64;
const MAX_RETIRED: usize = 128;
const MAX_ID_BYTES: usize = 512;
const MAX_ALIASES: usize = 64;
const MAX_ALIAS_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPromptProof {
    pub command_id: String,
    pub session_id: String,
    pub user_id: String,
    pub turn_id: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct User {
    session: String,
    turn: String,
    id: String,
    aliases: Vec<String>,
}

struct Pending {
    command: String,
    expected_session: Option<String>,
    accepted: Option<(Option<String>, String)>,
    user: Option<User>,
}

#[derive(Default)]
struct Joins {
    pending: BTreeMap<String, Pending>,
    retired: VecDeque<String>,
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty()
        && id.len() <= MAX_ID_BYTES
        && !id.chars().any(char::is_control)
}

fn aliases(values: &[String]) -> Option<Vec<String>> {
    if values.len() > MAX_ALIASES
        || values.iter().map(String::len).sum::<usize>() > MAX_ALIAS_BYTES
        || values.iter().any(|value| !valid_id(value))
    {
        return None;
    }
    let mut result = Vec::new();
    for value in values {
        if !result.contains(value) {
            result.push(value.clone());
        }
    }
    Some(result)
}

impl Joins {
    fn submit(&mut self, key: &str, command: &str, expected: Option<&str>) -> Result<(), String> {
        if !valid_id(key) || !valid_id(command) || expected.is_some_and(|id| !valid_id(id)) {
            return Err("Invalid original-command proof identity".into());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err("Too many pending original-command proofs".into());
        }
        if self.pending.contains_key(key)
            || self.retired.iter().any(|id| id == key)
            || self.pending.values().any(|pending| pending.command == command)
        {
            return Err("Original-command proof identity was already submitted".into());
        }
        self.pending.insert(key.into(), Pending {
            command: command.into(),
            expected_session: expected.map(str::to_owned),
            accepted: None,
            user: None,
        });
        Ok(())
    }

    fn retire(&mut self, key: &str) {
        if self.pending.remove(key).is_some() {
            self.retired.push_back(key.into());
            while self.retired.len() > MAX_RETIRED {
                self.retired.pop_front();
            }
        }
    }

    fn cancel(&mut self, command: &str) {
        let key = self.pending.iter().find_map(|(key, pending)| {
            (pending.command == command).then(|| key.clone())
        });
        if let Some(key) = key {
            self.retire(&key);
        }
    }

    fn accepted(&mut self, key: &str, session: Option<&str>, turn: &str) -> Option<AcceptedPromptProof> {
        if !valid_id(turn) || session.is_some_and(|id| !valid_id(id)) {
            return None;
        }
        let pending = self.pending.get_mut(key)?;
        if session.zip(pending.expected_session.as_deref()).is_some_and(|(a, b)| a != b) {
            return None;
        }
        let accepted = (session.map(str::to_owned), turn.to_owned());
        if let Some(previous) = &pending.accepted {
            if previous.1 != accepted.1
                || previous.0.as_deref().zip(session).is_some_and(|(a, b)| a != b)
            {
                self.retire(key);
                return None;
            }
        }
        // A later exact session confirmation may strengthen the same ACK.
        if pending.accepted.as_ref().is_none_or(|(session, _)| session.is_none()) {
            pending.accepted = Some(accepted);
        }
        self.finish(key)
    }

    fn user(&mut self, key: &str, mut user: User) -> Option<AcceptedPromptProof> {
        let pending = self.pending.get_mut(key)?;
        if pending.expected_session.as_deref().is_some_and(|expected| expected != user.session) {
            return None;
        }
        if let Some(previous) = &pending.user {
            if (previous.session.as_str(), previous.turn.as_str(), previous.id.as_str())
                != (user.session.as_str(), user.turn.as_str(), user.id.as_str())
            {
                self.retire(key);
                return None;
            }
            let mut combined = previous.aliases.clone();
            combined.extend(user.aliases);
            user.aliases = aliases(&combined)?;
        }
        pending.user = Some(user);
        self.finish(key)
    }

    fn finish(&mut self, key: &str) -> Option<AcceptedPromptProof> {
        let pending = self.pending.get(key)?;
        let (session, turn) = pending.accepted.as_ref()?;
        let user = pending.user.as_ref()?;
        if turn != &user.turn || session.as_deref().is_some_and(|session| session != user.session) {
            return None;
        }
        let proof = AcceptedPromptProof {
            command_id: pending.command.clone(),
            session_id: user.session.clone(),
            user_id: user.id.clone(),
            turn_id: user.turn.clone(),
            aliases: user.aliases.clone(),
        };
        self.retire(key);
        Some(proof)
    }
}

#[derive(Default)]
pub(crate) struct ClaudeProofs {
    joins: Joins,
}

impl ClaudeProofs {
    pub fn submit(&mut self, command: &str, uuid: &str, expected_session: Option<&str>) -> Result<(), String> {
        self.joins.submit(uuid, command, expected_session)
    }

    pub fn cancel(&mut self, command: &str) {
        self.joins.cancel(command);
    }

    /// The optional context must itself come from a provider-confirmed session,
    /// never launch argv or submit's expected session. A user frame supplies
    /// its own authority when context was not confirmed before the first prompt.
    pub fn observe(&mut self, frame: &Value, parsed_aliases: &[String], confirmed_session: Option<&str>) -> Option<AcceptedPromptProof> {
        let frame_session = frame["session_id"].as_str();
        if frame_session.zip(confirmed_session).is_some_and(|(a, b)| a != b) {
            return None;
        }
        let session = frame_session.or(confirmed_session);
        match frame["type"].as_str()? {
            "command_lifecycle" => {
                let key = frame["command_uuid"].as_str()?;
                match frame["state"].as_str()? {
                    "started" | "completed" => self.joins.accepted(key, session, key),
                    "cancelled" | "discarded" => {
                        self.joins.retire(key);
                        None
                    }
                    _ => None,
                }
            }
            "user" => {
                if !frame["parent_tool_use_id"].is_null()
                    || frame["isSynthetic"].as_bool() == Some(true)
                    || frame["message"]["role"] != "user"
                {
                    return None;
                }
                let key = frame["uuid"].as_str()?;
                let session = session.filter(|id| valid_id(id))?;
                self.joins.user(key, User {
                    session: session.into(),
                    turn: key.into(),
                    id: key.into(),
                    aliases: aliases(parsed_aliases)?,
                })
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct CodexProofs {
    joins: Joins,
    rpc: BTreeMap<String, String>,
}

impl CodexProofs {
    /// Register the exact turn/start request ID and clientUserMessageId written
    /// by the adapter. Queue-add success alone is not turn/start acceptance.
    pub fn submit(&mut self, command: &str, client_id: &str, rpc_id: &str, expected_session: Option<&str>) -> Result<(), String> {
        if !valid_id(rpc_id) || self.rpc.contains_key(rpc_id) {
            return Err("Invalid or reused original-command RPC identity".into());
        }
        self.joins.submit(client_id, command, expected_session)?;
        self.rpc.insert(rpc_id.into(), client_id.into());
        Ok(())
    }

    fn prune(&mut self) {
        self.rpc.retain(|_, key| self.joins.pending.contains_key(key));
    }

    pub fn cancel(&mut self, command: &str) {
        self.joins.cancel(command);
        self.prune();
    }

    /// A successful correlated turn/start response proves the turn, not the
    /// user identity. Only the actual UserMessage clientId completes the join.
    pub fn acknowledge(&mut self, frame: &Value, confirmed_session: Option<&str>) -> Option<AcceptedPromptProof> {
        let rpc_id = frame["id"].as_str()?;
        let key = self.rpc.get(rpc_id)?.clone();
        if frame.get("error").is_some_and(|error| !error.is_null()) {
            self.joins.retire(&key);
            self.prune();
            return None;
        }
        let turn = frame["result"]["turn"]["id"].as_str()?;
        let proof = self.joins.accepted(&key, confirmed_session, turn);
        self.prune();
        proof
    }

    /// The caller supplies the actual enclosing provider thread/turn and the
    /// aliases returned by parsing this native item. No suffix is reconstructed.
    pub fn user(&mut self, session: &str, turn: &str, item: &Value, parsed_aliases: &[String]) -> Option<AcceptedPromptProof> {
        if item["type"] != "userMessage" || !valid_id(session) || !valid_id(turn) {
            return None;
        }
        let key = item["clientId"].as_str().filter(|id| valid_id(id))?;
        let id = item["id"].as_str().filter(|id| valid_id(id))?;
        let proof = self.joins.user(key, User {
            session: session.into(),
            turn: turn.into(),
            id: id.into(),
            aliases: aliases(parsed_aliases)?,
        });
        self.prune();
        proof
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelEffort {
    pub model: String,
    pub effort: String,
}

fn unavailable() -> OriginalError {
    OriginalError::Unavailable("The provider did not report exact applied model and reasoning settings".into())
}

fn exact_value(value: &str) -> bool {
    valid_id(value)
        && !["default", "auto", "automatic", "unknown"]
            .iter()
            .any(|placeholder| value.eq_ignore_ascii_case(placeholder))
}

/// Parse only the inner successful get_settings response. Its request/session
/// correlation and freshness across dispatch belong to the adapter's revision
/// guard. Requested settings and merged file settings are not applied values.
pub(crate) fn claude_applied_model_effort(payload: &Value) -> Result<ModelEffort, OriginalError> {
    let model = payload["applied"]["model"].as_str().filter(|id| exact_value(id)).ok_or_else(unavailable)?;
    let effort = payload["applied"]["effort"].as_str().filter(|effort| matches!(*effort, "low" | "medium" | "high" | "xhigh" | "max")).ok_or_else(unavailable)?;
    Ok(ModelEffort { model: model.into(), effort: effort.into() })
}

/// Thread start/resume responses report resolved model/reasoningEffort. A
/// turn/start ACK, local selection, null/default effort or missing report does
/// not. The caller must independently establish this response's current scope.
pub(crate) fn codex_reported_model_effort(payload: &Value) -> Result<ModelEffort, OriginalError> {
    if !payload["thread"]["id"].as_str().is_some_and(valid_id) {
        return Err(unavailable());
    }
    let model = payload["model"].as_str().filter(|id| exact_value(id)).ok_or_else(unavailable)?;
    // Installed app-server schema defines ReasoningEffort as a nonempty
    // advertised string, not a fixed enum. Preserve its actual reported value.
    let effort = payload["reasoningEffort"].as_str().filter(|effort| exact_value(effort)).ok_or_else(unavailable)?;
    Ok(ModelEffort { model: model.into(), effort: effort.into() })
}

/// Combine fresh applied values with an actual matching-session SDK init.
/// Missing fields, cooldown and null effort remain unavailable, not defaults.
pub(crate) fn claude_reported_settings(payload: &Value, init: &Value, confirmed_session: &str) -> Result<OriginalSettings, OriginalError> {
    let applied = claude_applied_model_effort(payload)?;
    if init["type"] != "system" || init["subtype"] != "init"
        || init["session_id"].as_str() != Some(confirmed_session)
        || init["model"].as_str() != Some(applied.model.as_str())
    {
        return Err(unavailable());
    }
    if init.get("effort").is_some_and(|effort| effort.as_str() != Some(applied.effort.as_str())) {
        return Err(unavailable());
    }
    let fast = match init["fast_mode_state"].as_str() {
        Some("on") => true,
        Some("off") => false,
        _ => return Err(unavailable()),
    };
    let permission = match init["permissionMode"].as_str() {
        Some("default") => "manual",
        Some(mode @ ("acceptEdits" | "auto" | "plan" | "dontAsk" | "bypassPermissions")) => mode,
        _ => return Err(unavailable()),
    };
    Ok(OriginalSettings {
        model: Setting::Value(applied.model),
        reasoning: Setting::Value(applied.effort),
        fast: Setting::Value(fast),
        permission: Setting::Value(permission.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_user(id: &str, session: &str) -> Value {
        json!({"type":"user","uuid":id,"session_id":session,
            "parent_tool_use_id":null,"message":{"id":"different-parser-root",
            "role":"user","content":"provider transformed echo"}})
    }

    fn lifecycle(id: &str, state: &str) -> Value {
        json!({"type":"command_lifecycle","command_uuid":id,"state":state})
    }

    fn codex_user(client: &str, id: &str) -> Value {
        json!({"type":"userMessage","clientId":client,"id":id,
            "content":[{"type":"text","text":"identical input"}]})
    }

    fn ack(id: &str, turn: &str) -> Value {
        json!({"id":id,"result":{"turn":{"id":turn,"status":"inProgress","items":[]}}})
    }

    #[test]
    fn claude_first_prompt_needs_real_user_and_lifecycle_in_either_order() {
        for echo_first in [false, true] {
            let mut proofs = ClaudeProofs::default();
            proofs.submit("original-command", "outbound-uuid", None).unwrap();
            // A local execution event has no provider acceptance authority.
            assert!(proofs.observe(&json!({"type":"Started","id":"outbound-uuid"}), &[], None).is_none());
            let user = claude_user("outbound-uuid", "actual-native-session");
            let aliases = vec!["different-parser-root/input/0".into(), "actual-second-part".into()];
            let result = if echo_first {
                assert!(proofs.observe(&user, &aliases, None).is_none());
                proofs.observe(&lifecycle("outbound-uuid", "started"), &[], None)
            } else {
                assert!(proofs.observe(&lifecycle("outbound-uuid", "completed"), &[], None).is_none());
                proofs.observe(&user, &aliases, None)
            }.unwrap();
            assert_eq!(result.command_id, "original-command");
            assert_eq!(result.session_id, "actual-native-session");
            assert_eq!(result.user_id, "outbound-uuid");
            assert_eq!(result.turn_id, "outbound-uuid");
            assert_eq!(result.aliases, aliases);
            assert!(proofs.observe(&user, &[], None).is_none());
            assert!(proofs.observe(&lifecycle("outbound-uuid", "completed"), &[], None).is_none());
        }
    }

    #[test]
    fn claude_expected_session_is_only_a_guard_and_cancelled_input_never_accepts() {
        for state in ["cancelled", "discarded"] {
            let mut proofs = ClaudeProofs::default();
            proofs.submit("command", "uuid", Some("expected")).unwrap();
            assert!(proofs.observe(&lifecycle("uuid", "started"), &[], None).is_none());
            let mut missing = claude_user("uuid", "expected");
            missing.as_object_mut().unwrap().remove("session_id");
            assert!(proofs.observe(&missing, &[], None).is_none());
            assert!(proofs.observe(&claude_user("uuid", "foreign"), &[], None).is_none());
            assert!(proofs.observe(&lifecycle("uuid", state), &[], None).is_none());
            assert!(proofs.observe(&claude_user("uuid", "expected"), &[], Some("expected")).is_none());
            assert!(proofs.submit("retry-with-reused-id", "uuid", Some("expected")).is_err());
        }
        let mut proofs = ClaudeProofs::default();
        proofs.submit("command", "uuid", Some("session")).unwrap();
        proofs.cancel("command");
        assert!(proofs.observe(&lifecycle("uuid", "started"), &[], Some("session")).is_none());
        assert!(proofs.observe(&claude_user("uuid", "session"), &[], None).is_none());
    }

    #[test]
    fn claude_identical_queued_prompts_remain_distinct_and_child_echoes_cannot_bind() {
        let mut proofs = ClaudeProofs::default();
        for id in ["first", "second"] {
            proofs.submit(&format!("command-{id}"), id, Some("session")).unwrap();
            assert!(proofs.observe(&lifecycle(id, "queued"), &[], Some("session")).is_none());
        }
        let mut child = claude_user("first", "session");
        child["parent_tool_use_id"] = json!("child-tool");
        assert!(proofs.observe(&child, &[], None).is_none());
        assert!(proofs.observe(&lifecycle("second", "started"), &[], Some("session")).is_none());
        let second = proofs.observe(&claude_user("second", "session"), &["opaque:part".into()], None).unwrap();
        assert_eq!(second.command_id, "command-second");
        assert!(proofs.observe(&claude_user("first", "session"), &["root-a".into()], None).is_none());
        assert!(proofs.observe(&claude_user("first", "session"), &["root-b".into()], None).is_none());
        let first = proofs.observe(&lifecycle("first", "started"), &[], None).unwrap();
        assert_eq!(first.command_id, "command-first");
        assert_eq!(first.aliases, vec!["root-a", "root-b"]);
    }

    #[test]
    fn codex_requires_correlated_ack_and_native_client_id_not_text_or_projection() {
        for user_first in [false, true] {
            let mut proofs = CodexProofs::default();
            proofs.submit("command", "client-id", "rpc-id", Some("thread")).unwrap();
            assert!(proofs.acknowledge(&ack("other-rpc", "turn"), Some("thread")).is_none());
            assert!(proofs.user("thread", "turn", &codex_user("other-client", "native-user"), &[]).is_none());
            let item = codex_user("client-id", "native-user");
            let aliases = vec!["opaque-parser-id".into()];
            let result = if user_first {
                assert!(proofs.user("thread", "turn", &item, &aliases).is_none());
                proofs.acknowledge(&ack("rpc-id", "turn"), Some("thread"))
            } else {
                assert!(proofs.acknowledge(&ack("rpc-id", "turn"), Some("thread")).is_none());
                proofs.user("thread", "turn", &item, &aliases)
            }.unwrap();
            assert_eq!(result.command_id, "command");
            assert_eq!(result.user_id, "native-user");
            assert_eq!(result.turn_id, "turn");
            assert_eq!(result.aliases, aliases);
            assert!(proofs.user("thread", "turn", &item, &[]).is_none());
            assert!(proofs.acknowledge(&ack("rpc-id", "turn"), Some("thread")).is_none());
        }
    }

    #[test]
    fn codex_error_queue_ack_absent_client_id_and_foreign_thread_do_not_accept() {
        let mut proofs = CodexProofs::default();
        proofs.submit("command", "client", "rpc", Some("thread")).unwrap();
        assert!(proofs.acknowledge(&json!({"id":"rpc","result":{"queuedUserMessageId":"queued"}}), Some("thread")).is_none());
        assert!(proofs.acknowledge(&ack("rpc", "turn"), Some("foreign-thread")).is_none());
        let mut item = codex_user("client", "native-user");
        item.as_object_mut().unwrap().remove("clientId");
        assert!(proofs.user("thread", "turn", &item, &[]).is_none());
        assert!(proofs.user("foreign-thread", "turn", &codex_user("client", "native-user"), &[]).is_none());
        assert!(proofs.acknowledge(&json!({"id":"rpc","error":{"message":"rejected"}}), Some("thread")).is_none());
        assert!(proofs.user("thread", "turn", &codex_user("client", "native-user"), &[]).is_none());
        proofs.submit("next-command", "next-client", "next-rpc", None).unwrap();
        proofs.cancel("next-command");
        assert!(proofs.acknowledge(&ack("next-rpc", "turn"), Some("thread")).is_none());
        assert!(proofs.user("thread", "turn", &codex_user("next-client", "native-user"), &[]).is_none());
    }

    #[test]
    fn proof_storage_and_aliases_are_bounded_without_guessing_missing_metadata() {
        let mut proofs = ClaudeProofs::default();
        for index in 0..MAX_PENDING {
            proofs.submit(&format!("command-{index}"), &format!("uuid-{index}"), None).unwrap();
        }
        assert!(proofs.submit("overflow", "overflow", None).is_err());
        proofs.cancel("command-0");
        proofs.submit("replacement", "replacement", None).unwrap();
        let aliases = vec!["display".to_owned(); MAX_ALIASES + 1];
        assert!(proofs.observe(&claude_user("replacement", "session"), &aliases, None).is_none());
        assert!(proofs.observe(&lifecycle("replacement", "started"), &[], None).is_none());
        assert!(proofs.observe(&claude_user("replacement", "session"), &[], None).is_some());
    }

    #[test]
    fn only_actual_applied_model_effort_and_matching_reported_metadata_are_restorable() {
        let applied = json!({"settings":{"model":"requested","effortLevel":"max"},
            "applied":{"model":"resolved-model","effort":"low"}});
        let init = json!({"type":"system","subtype":"init","session_id":"session",
            "model":"resolved-model","permissionMode":"default","fast_mode_state":"off"});
        let settings = claude_reported_settings(&applied, &init, "session").unwrap();
        assert!(settings.is_exact());
        assert_eq!(settings.model, Setting::Value("resolved-model".into()));
        assert_eq!(settings.reasoning, Setting::Value("low".into()));
        assert_eq!(settings.permission, Setting::Value("manual".into()));
        assert_eq!(settings.fast, Setting::Value(false));
        for invalid in [json!({"settings":{"model":"requested","effortLevel":"low"}}),
            json!({"applied":{"model":"resolved-model","effort":null}}),
            json!({"applied":{"model":"resolved-model"}})] {
            assert!(matches!(claude_reported_settings(&invalid, &init, "session"), Err(OriginalError::Unavailable(_))));
        }
        for key in ["fast_mode_state", "permissionMode", "session_id", "model"] {
            let mut missing = init.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(claude_reported_settings(&applied, &missing, "session").is_err());
        }
        for effort in [Value::Null, json!("high")] {
            let mut mismatched = init.clone();
            mismatched["effort"] = effort;
            assert!(claude_reported_settings(&applied, &mismatched, "session").is_err());
        }
        let mut cooldown = init;
        cooldown["fast_mode_state"] = json!("cooldown");
        assert!(claude_reported_settings(&applied, &cooldown, "session").is_err());
        assert!(claude_applied_model_effort(&json!({"applied":{"model":"default","effort":"low"}})).is_err());
        let codex = codex_reported_model_effort(&json!({"thread":{"id":"thread"},"model":"codex-model","reasoningEffort":"future-advertised-effort"})).unwrap();
        assert_eq!(codex.effort, "future-advertised-effort");
        for effort in [Value::Null, json!("default")] {
            assert!(codex_reported_model_effort(&json!({"thread":{"id":"thread"},"model":"codex-model","reasoningEffort":effort})).is_err());
        }
        assert!(codex_reported_model_effort(&json!({"params":{"model":"requested","effort":"low"}})).is_err());
        assert!(codex_reported_model_effort(&ack("rpc", "turn")["result"]).is_err());
    }
}
