//! Codex's persisted thread-goal RPCs, not terminal slash-command emulation.
//!
//! Request IDs, physical admission and observation ordering belong to the
//! adapter. This codec never starts a turn, retries a mutation or chooses a
//! thread. Its contract was checked against codex-cli 0.160.0's stable schemas.
use crate::chat::goal::{Goal, Operation, Reply};
use serde_json::{Value, json};

pub(super) fn request(
    thread: &str,
    operation: &Operation,
) -> Result<(&'static str, Value), String> {
    if thread.is_empty() || thread == "latest" {
        return Err("Goal controls require an exact Codex thread".into());
    }
    let mut params = json!({"threadId": thread});
    let method = match operation {
        Operation::Get => "thread/goal/get",
        Operation::SetObjective(objective) => {
            operation.validate().map_err(|error| error.to_string())?;
            params["objective"] = json!(objective);
            "thread/goal/set"
        }
        Operation::Pause | Operation::Resume => {
            params["status"] = json!(if matches!(operation, Operation::Pause) {
                "paused"
            } else {
                "active"
            });
            "thread/goal/set"
        }
        Operation::Clear => "thread/goal/clear",
    };
    // In particular, status-only changes omit objective and tokenBudget. An
    // absent budget preserves the provider's policy; null is not substituted.
    Ok((method, params))
}

fn observed_goal(value: &Value, thread: &str) -> Result<Goal, String> {
    // Deserialize only after checking the bounded objective text; the transport
    // bounds the complete JSON frame independently.
    let objective = value["objective"]
        .as_str()
        .ok_or("Codex returned a goal without an objective")?;
    crate::chat::goal::validated_objective(objective).map_err(|error| error.to_string())?;
    let goal: Goal = serde_json::from_value(value.clone())
        .map_err(|_| "Codex returned an invalid goal observation".to_owned())?;
    goal.validate(thread).map_err(|error| error.to_string())?;
    Ok(goal)
}

pub(super) fn response(
    thread: &str,
    operation: &Operation,
    result: &Value,
) -> Result<Reply, String> {
    if !result.is_object() {
        return Err("Codex returned an invalid goal response".into());
    }
    match operation {
        Operation::Clear => result["cleared"]
            .as_bool()
            .map(Reply::Cleared)
            .ok_or_else(|| "Codex did not confirm the goal clear result".into()),
        Operation::Get if result.get("goal").is_none_or(Value::is_null) => Ok(Reply::Read(None)),
        Operation::Get => {
            observed_goal(&result["goal"], thread).map(|goal| Reply::Read(Some(goal)))
        }
        _ => observed_goal(&result["goal"], thread).map(Reply::Updated),
    }
}

/// Only the exact outer thread may update this actor. For updated events the
/// goal's inner thread must agree too. Child and unrelated events remain for
/// their own actor; an invalid current-thread event is reported, not accepted.
pub(super) fn notification(
    thread: &str,
    method: &str,
    params: &Value,
) -> Result<Option<Option<Goal>>, String> {
    if !matches!(method, "thread/goal/updated" | "thread/goal/cleared") {
        return Ok(None);
    }
    let outer = params["threadId"]
        .as_str()
        .ok_or("Codex returned a goal event without a thread")?;
    if outer != thread {
        return Ok(None);
    }
    if method == "thread/goal/cleared" {
        return Ok(Some(None));
    }
    observed_goal(&params["goal"], thread).map(|goal| Some(Some(goal)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(thread: &str, status: &str) -> Value {
        json!({
            "threadId": thread, "objective": "Finish the reviewed change",
            "status": status, "tokenBudget": null, "tokensUsed": 17,
            "timeUsedSeconds": 4, "createdAt": 1, "updatedAt": 2
        })
    }

    #[test]
    fn native_controls_use_exact_thread_and_never_create_a_prompt_or_budget() {
        for (operation, method, extra) in [
            (Operation::Get, "thread/goal/get", json!({})),
            (
                Operation::SetObjective("  Keep Unicode 🦀 and whitespace  ".into()),
                "thread/goal/set",
                json!({"objective":"  Keep Unicode 🦀 and whitespace  "}),
            ),
            (
                Operation::Pause,
                "thread/goal/set",
                json!({"status":"paused"}),
            ),
            (
                Operation::Resume,
                "thread/goal/set",
                json!({"status":"active"}),
            ),
            (Operation::Clear, "thread/goal/clear", json!({})),
        ] {
            let (actual_method, params) = request("original-thread", &operation).unwrap();
            let mut expected = extra;
            expected["threadId"] = json!("original-thread");
            assert_eq!(actual_method, method);
            assert_eq!(params, expected);
        }
        assert!(request("latest", &Operation::Get).is_err());
        assert!(request("", &Operation::Get).is_err());
    }

    #[test]
    fn direct_operations_cannot_bypass_unicode_objective_validation() {
        let accepted = "🦀".repeat(4_000);
        assert!(request("thread", &Operation::SetObjective(accepted)).is_ok());
        for objective in ["🦀".repeat(4_001), " \t\n".into(), String::new()] {
            assert!(request("thread", &Operation::SetObjective(objective)).is_err());
        }
    }

    #[test]
    fn read_absence_is_distinct_from_malformed_set_and_unconfirmed_clear() {
        for result in [json!({}), json!({"goal":null})] {
            assert_eq!(
                response("thread", &Operation::Get, &result).unwrap(),
                Reply::Read(None)
            );
            assert!(response("thread", &Operation::Pause, &result).is_err());
        }
        for cleared in [false, true] {
            assert_eq!(
                response("thread", &Operation::Clear, &json!({"cleared":cleared})).unwrap(),
                Reply::Cleared(cleared)
            );
        }
        for invalid in [Value::Null, json!([]), json!({"cleared":"true"})] {
            assert!(response("thread", &Operation::Clear, &invalid).is_err());
        }
    }

    #[test]
    fn every_native_status_preserves_accounting_without_timestamp_ordering() {
        for status in [
            "active",
            "paused",
            "blocked",
            "usageLimited",
            "budgetLimited",
            "complete",
        ] {
            let mut raw = goal("thread", status);
            // Neither generated schema promises timestamp units or monotonic
            // order. The actor's local observation fence supplies ordering.
            raw["createdAt"] = json!(50);
            raw["updatedAt"] = json!(2);
            let Reply::Read(Some(actual)) =
                response("thread", &Operation::Get, &json!({"goal":raw})).unwrap()
            else {
                panic!("expected a typed observation")
            };
            assert_eq!(serde_json::to_value(actual).unwrap(), raw);
        }
    }

    #[test]
    fn wrong_thread_and_future_or_incomplete_goals_cannot_replace_observed_state() {
        assert!(
            response(
                "thread",
                &Operation::Get,
                &json!({"goal":goal("child","active")})
            )
            .is_err()
        );
        let mut invalid = goal("thread", "futureStatus");
        assert!(response("thread", &Operation::Get, &json!({"goal":invalid})).is_err());
        invalid["status"] = json!("active");
        invalid.as_object_mut().unwrap().remove("tokensUsed");
        assert!(response("thread", &Operation::Get, &json!({"goal":invalid})).is_err());
        invalid = goal("thread", "active");
        invalid["objective"] = json!("a".repeat(4_001));
        assert!(response("thread", &Operation::Get, &json!({"goal":invalid})).is_err());
    }

    #[test]
    fn notifications_require_both_original_thread_identities() {
        let params = |outer: &str, inner: &str| json!({"threadId":outer,"goal":goal(inner,"paused"),"turnId":null});
        assert!(
            notification("thread", "thread/goal/updated", &params("thread", "thread"))
                .unwrap()
                .flatten()
                .is_some()
        );
        assert_eq!(
            notification("thread", "thread/goal/updated", &params("child", "child")).unwrap(),
            None
        );
        assert!(notification("thread", "thread/goal/updated", &params("thread", "child")).is_err());
        assert_eq!(
            notification(
                "thread",
                "thread/goal/cleared",
                &json!({"threadId":"thread"})
            )
            .unwrap(),
            Some(None)
        );
        assert_eq!(
            notification(
                "thread",
                "thread/goal/cleared",
                &json!({"threadId":"child"})
            )
            .unwrap(),
            None
        );
        assert!(notification("thread", "thread/goal/cleared", &json!({})).is_err());
        assert_eq!(notification("thread", "other", &json!({})).unwrap(), None);
    }
}
