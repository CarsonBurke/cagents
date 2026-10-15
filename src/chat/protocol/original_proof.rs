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
pub(crate) use super::input_proof::{AcceptedPromptProof, ClaudeProofs, CodexProofs};
use super::input_proof::valid_id;
#[cfg(test)]
use super::input_proof::{MAX_PENDING, MAX_ALIASES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelEffort {
    pub model: String,
    pub effort: String,
}

fn unavailable() -> OriginalError {
    OriginalError::Unavailable(
        "The provider did not report exact applied model and reasoning settings".into(),
    )
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
    let model = payload["applied"]["model"]
        .as_str()
        .filter(|id| exact_value(id))
        .ok_or_else(unavailable)?;
    let effort = payload["applied"]["effort"]
        .as_str()
        .filter(|effort| matches!(*effort, "low" | "medium" | "high" | "xhigh" | "max"))
        .ok_or_else(unavailable)?;
    Ok(ModelEffort {
        model: model.into(),
        effort: effort.into(),
    })
}

/// Thread start/resume responses report resolved model/reasoningEffort. A
/// turn/start ACK, local selection, null/default effort or missing report does
/// not. The caller must independently establish this response's current scope.
pub(crate) fn codex_reported_model_effort(payload: &Value) -> Result<ModelEffort, OriginalError> {
    if !payload["thread"]["id"].as_str().is_some_and(valid_id) {
        return Err(unavailable());
    }
    let model = payload["model"]
        .as_str()
        .filter(|id| exact_value(id))
        .ok_or_else(unavailable)?;
    // Installed app-server schema defines ReasoningEffort as a nonempty
    // advertised string, not a fixed enum. Preserve its actual reported value.
    let effort = payload["reasoningEffort"]
        .as_str()
        .filter(|effort| exact_value(effort))
        .ok_or_else(unavailable)?;
    Ok(ModelEffort {
        model: model.into(),
        effort: effort.into(),
    })
}

/// Combine fresh applied values with an actual matching-session SDK init.
/// Missing fields, cooldown and null effort remain unavailable, not defaults.
pub(crate) fn claude_reported_settings(
    payload: &Value,
    init: &Value,
    confirmed_session: &str,
) -> Result<OriginalSettings, OriginalError> {
    let applied = claude_applied_model_effort(payload)?;
    if init["type"] != "system"
        || init["subtype"] != "init"
        || init["session_id"].as_str() != Some(confirmed_session)
        || init["model"].as_str() != Some(applied.model.as_str())
    {
        return Err(unavailable());
    }
    if init
        .get("effort")
        .is_some_and(|effort| effort.as_str() != Some(applied.effort.as_str()))
    {
        return Err(unavailable());
    }
    claude_applied_settings(
        payload,
        init["permissionMode"].as_str().ok_or_else(unavailable)?,
        init["fast_mode_state"].as_str().ok_or_else(unavailable)?,
    )
}

/// Fresh applied model/effort plus provider-reported policy on this exact
/// connection. Caller must retain ordering/revision proof through dispatch;
/// requested flags and later initialization frames cannot fill missing values.
pub(crate) fn claude_applied_settings(
    payload: &Value,
    permission: &str,
    fast_state: &str,
) -> Result<OriginalSettings, OriginalError> {
    let applied = claude_applied_model_effort(payload)?;
    let fast = match fast_state {
        "on" => true,
        "off" => false,
        _ => return Err(unavailable()),
    };
    let permission = match permission {
        "default" => "manual",
        mode @ ("manual" | "acceptEdits" | "auto" | "plan" | "dontAsk" | "bypassPermissions") => {
            mode
        }
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
            proofs
                .submit("original-command", "outbound-uuid", None)
                .unwrap();
            // A local execution event has no provider acceptance authority.
            assert!(
                proofs
                    .observe(&json!({"type":"Started","id":"outbound-uuid"}), &[], None)
                    .is_none()
            );
            let user = claude_user("outbound-uuid", "actual-native-session");
            let aliases = vec![
                "different-parser-root/input/0".into(),
                "actual-second-part".into(),
            ];
            let result = if echo_first {
                assert!(proofs.observe(&user, &aliases, None).is_none());
                proofs.observe(&lifecycle("outbound-uuid", "started"), &[], None)
            } else {
                assert!(
                    proofs
                        .observe(&lifecycle("outbound-uuid", "completed"), &[], None)
                        .is_none()
                );
                proofs.observe(&user, &aliases, None)
            }
            .unwrap();
            assert_eq!(result.command_id, "original-command");
            assert_eq!(result.session_id, "actual-native-session");
            assert_eq!(result.user_id, "outbound-uuid");
            assert_eq!(result.turn_id, "outbound-uuid");
            assert_eq!(result.aliases, aliases);
            assert!(proofs.observe(&user, &[], None).is_none());
            assert!(
                proofs
                    .observe(&lifecycle("outbound-uuid", "completed"), &[], None)
                    .is_none()
            );
        }
    }

    #[test]
    fn claude_expected_session_is_only_a_guard_and_cancelled_input_never_accepts() {
        for state in ["cancelled", "discarded"] {
            let mut proofs = ClaudeProofs::default();
            proofs.submit("command", "uuid", Some("expected")).unwrap();
            assert!(
                proofs
                    .observe(&lifecycle("uuid", "started"), &[], None)
                    .is_none()
            );
            let mut missing = claude_user("uuid", "expected");
            missing.as_object_mut().unwrap().remove("session_id");
            assert!(proofs.observe(&missing, &[], None).is_none());
            assert!(
                proofs
                    .observe(&claude_user("uuid", "foreign"), &[], None)
                    .is_none()
            );
            assert!(
                proofs
                    .observe(&lifecycle("uuid", state), &[], None)
                    .is_none()
            );
            assert!(
                proofs
                    .observe(&claude_user("uuid", "expected"), &[], Some("expected"))
                    .is_none()
            );
            assert!(
                proofs
                    .submit("retry-with-reused-id", "uuid", Some("expected"))
                    .is_err()
            );
        }
        let mut proofs = ClaudeProofs::default();
        proofs.submit("command", "uuid", Some("session")).unwrap();
        proofs.cancel("command");
        assert!(
            proofs
                .observe(&lifecycle("uuid", "started"), &[], Some("session"))
                .is_none()
        );
        assert!(
            proofs
                .observe(&claude_user("uuid", "session"), &[], None)
                .is_none()
        );
    }

    #[test]
    fn claude_identical_queued_prompts_remain_distinct_and_child_echoes_cannot_bind() {
        let mut proofs = ClaudeProofs::default();
        for id in ["first", "second"] {
            proofs
                .submit(&format!("command-{id}"), id, Some("session"))
                .unwrap();
            assert!(
                proofs
                    .observe(&lifecycle(id, "queued"), &[], Some("session"))
                    .is_none()
            );
        }
        let mut child = claude_user("first", "session");
        child["parent_tool_use_id"] = json!("child-tool");
        assert!(proofs.observe(&child, &[], None).is_none());
        assert!(
            proofs
                .observe(&lifecycle("second", "started"), &[], Some("session"))
                .is_none()
        );
        let second = proofs
            .observe(
                &claude_user("second", "session"),
                &["opaque:part".into()],
                None,
            )
            .unwrap();
        assert_eq!(second.command_id, "command-second");
        assert!(
            proofs
                .observe(&claude_user("first", "session"), &["root-a".into()], None)
                .is_none()
        );
        assert!(
            proofs
                .observe(&claude_user("first", "session"), &["root-b".into()], None)
                .is_none()
        );
        let first = proofs
            .observe(&lifecycle("first", "started"), &[], None)
            .unwrap();
        assert_eq!(first.command_id, "command-first");
        assert_eq!(first.aliases, vec!["root-a", "root-b"]);
    }

    #[test]
    fn codex_requires_correlated_ack_and_native_client_id_not_text_or_projection() {
        for user_first in [false, true] {
            let mut proofs = CodexProofs::default();
            proofs
                .submit("command", "client-id", "rpc-id", Some("thread"))
                .unwrap();
            assert!(
                proofs
                    .acknowledge(&ack("other-rpc", "turn"), Some("thread"))
                    .is_none()
            );
            assert!(
                proofs
                    .user(
                        "thread",
                        "turn",
                        &codex_user("other-client", "native-user"),
                        &[]
                    )
                    .is_none()
            );
            let item = codex_user("client-id", "native-user");
            let aliases = vec!["opaque-parser-id".into()];
            let result = if user_first {
                assert!(proofs.user("thread", "turn", &item, &aliases).is_none());
                proofs.acknowledge(&ack("rpc-id", "turn"), Some("thread"))
            } else {
                assert!(
                    proofs
                        .acknowledge(&ack("rpc-id", "turn"), Some("thread"))
                        .is_none()
                );
                proofs.user("thread", "turn", &item, &aliases)
            }
            .unwrap();
            assert_eq!(result.command_id, "command");
            assert_eq!(result.user_id, "native-user");
            assert_eq!(result.turn_id, "turn");
            assert_eq!(result.aliases, aliases);
            assert!(proofs.user("thread", "turn", &item, &[]).is_none());
            assert!(
                proofs
                    .acknowledge(&ack("rpc-id", "turn"), Some("thread"))
                    .is_none()
            );
        }
    }

    #[test]
    fn codex_error_queue_ack_absent_client_id_and_foreign_thread_do_not_accept() {
        let mut proofs = CodexProofs::default();
        proofs
            .submit("command", "client", "rpc", Some("thread"))
            .unwrap();
        assert!(
            proofs
                .acknowledge(
                    &json!({"id":"rpc","result":{"queuedUserMessageId":"queued"}}),
                    Some("thread")
                )
                .is_none()
        );
        assert!(
            proofs
                .acknowledge(&ack("rpc", "turn"), Some("foreign-thread"))
                .is_none()
        );
        let mut item = codex_user("client", "native-user");
        item.as_object_mut().unwrap().remove("clientId");
        assert!(proofs.user("thread", "turn", &item, &[]).is_none());
        assert!(
            proofs
                .user(
                    "foreign-thread",
                    "turn",
                    &codex_user("client", "native-user"),
                    &[]
                )
                .is_none()
        );
        assert!(
            proofs
                .acknowledge(
                    &json!({"id":"rpc","error":{"message":"rejected"}}),
                    Some("thread")
                )
                .is_none()
        );
        assert!(
            proofs
                .user("thread", "turn", &codex_user("client", "native-user"), &[])
                .is_none()
        );
        proofs
            .submit("next-command", "next-client", "next-rpc", None)
            .unwrap();
        proofs.cancel("next-command");
        assert!(
            proofs
                .acknowledge(&ack("next-rpc", "turn"), Some("thread"))
                .is_none()
        );
        assert!(
            proofs
                .user(
                    "thread",
                    "turn",
                    &codex_user("next-client", "native-user"),
                    &[]
                )
                .is_none()
        );
    }

    #[test]
    fn proof_storage_and_aliases_are_bounded_without_guessing_missing_metadata() {
        let mut proofs = ClaudeProofs::default();
        for index in 0..MAX_PENDING {
            proofs
                .submit(&format!("command-{index}"), &format!("uuid-{index}"), None)
                .unwrap();
        }
        assert!(proofs.submit("overflow", "overflow", None).is_err());
        proofs.cancel("command-0");
        proofs.submit("replacement", "replacement", None).unwrap();
        let aliases = vec!["display".to_owned(); MAX_ALIASES + 1];
        assert!(
            proofs
                .observe(&claude_user("replacement", "session"), &aliases, None)
                .is_none()
        );
        assert!(
            proofs
                .observe(&lifecycle("replacement", "started"), &[], None)
                .is_none()
        );
        assert!(
            proofs
                .observe(&claude_user("replacement", "session"), &[], None)
                .is_some()
        );
    }

    #[test]
    fn claude_refused_commands_release_capacity_and_late_echo_cannot_accept() {
        let mut proofs = ClaudeProofs::default();
        for index in 0..MAX_PENDING {
            proofs
                .submit(
                    &format!("command-{index}"),
                    &format!("uuid-{index}"),
                    Some("session"),
                )
                .unwrap();
        }
        for index in 0..MAX_PENDING {
            let id = format!("uuid-{index}");
            assert!(
                proofs
                    .observe(&lifecycle(&id, "refused"), &[], Some("session"))
                    .is_none()
            );
            assert!(
                proofs
                    .observe(&lifecycle(&id, "started"), &[], Some("session"))
                    .is_none()
            );
            assert!(
                proofs
                    .observe(&claude_user(&id, "session"), &[], None)
                    .is_none()
            );
        }
        proofs
            .submit("new-command", "new-uuid", Some("session"))
            .unwrap();
        assert!(
            proofs
                .observe(&lifecycle("new-uuid", "started"), &[], Some("session"))
                .is_none()
        );
        assert!(
            proofs
                .observe(&claude_user("new-uuid", "session"), &[], None)
                .is_some()
        );
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
        for invalid in [
            json!({"settings":{"model":"requested","effortLevel":"low"}}),
            json!({"applied":{"model":"resolved-model","effort":null}}),
            json!({"applied":{"model":"resolved-model"}}),
        ] {
            assert!(matches!(
                claude_reported_settings(&invalid, &init, "session"),
                Err(OriginalError::Unavailable(_))
            ));
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
        assert!(
            claude_applied_model_effort(&json!({"applied":{"model":"default","effort":"low"}}))
                .is_err()
        );
        let codex = codex_reported_model_effort(&json!({"thread":{"id":"thread"},"model":"codex-model","reasoningEffort":"future-advertised-effort"})).unwrap();
        assert_eq!(codex.effort, "future-advertised-effort");
        for effort in [Value::Null, json!("default")] {
            assert!(codex_reported_model_effort(&json!({"thread":{"id":"thread"},"model":"codex-model","reasoningEffort":effort})).is_err());
        }
        assert!(
            codex_reported_model_effort(&json!({"params":{"model":"requested","effort":"low"}}))
                .is_err()
        );
        assert!(codex_reported_model_effort(&ack("rpc", "turn")["result"]).is_err());
    }
}
