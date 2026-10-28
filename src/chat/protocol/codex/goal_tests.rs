//! Real adapter request/response fixtures. No app-server or provider is started.
use super::*;
use crate::chat::goal::{Operation, ReadState, Snapshot, Status, Support};

fn adapter() -> Codex {
    // Codex owns alias cleanup through Drop; struct update cannot move its
    // non-Copy fields out of a temporary default value.
    let mut adapter = Codex::default();
    adapter.thread = Some("thread-exact".into());
    adapter.cwd = PathBuf::from("/tmp");
    adapter.expected_home = std::fs::canonicalize("/tmp").unwrap();
    adapter.ready = true;
    adapter.attached = true;
    adapter.direct_input = true;
    adapter.goal_resume_admission = GoalResumeAdmission::OriginalScope;
    adapter
}

fn raw_goal(thread: &str, objective: &str, status: &str) -> Value {
    json!({"threadId":thread,"objective":objective,"status":status,
        "tokenBudget":null,"tokensUsed":17,"timeUsedSeconds":4,
        "createdAt":50,"updatedAt":2})
}

fn snapshot(adapter: &mut Codex) -> Snapshot {
    adapter.goal_state().unwrap().snapshot()
}

fn published(step: &Step) -> &Snapshot {
    step.events
        .iter()
        .find_map(|event| match event {
            Event::Goal(snapshot) => Some(snapshot),
            _ => None,
        })
        .expect("actual adapter must publish its goal observation")
}

fn command(adapter: &mut Codex, operation: Operation) -> Step {
    let observed = snapshot(adapter);
    adapter
        .command(Command::Goal {
            thread_id: observed.thread_id,
            revision: observed.revision,
            operation,
        })
        .unwrap()
}

fn select_goal_model(adapter: &mut Codex) {
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"provider-default","effort":"medium"}
    }})).unwrap();
    adapter.models = vec![
        Model {
            id: "provider-default".into(),
            label: "Original model".into(),
            reasoning: vec!["medium".into()],
        },
        Model {
            id: "explicit-goal-model".into(),
            label: "Explicit Goal model".into(),
            reasoning: vec!["low".into()],
        },
    ];
    let selected = adapter
        .command(Command::SetModel {
            model: "explicit-goal-model".into(),
            reasoning: Some("low".into()),
        })
        .unwrap();
    assert!(
        selected.writes.is_empty(),
        "SetModel only stages turn input"
    );
}

#[test]
fn autonomous_goal_work_applies_explicit_model_and_effort_before_the_goal_rpc() {
    for operation in [
        Operation::SetObjective("Use my selected model".into()),
        Operation::Resume,
    ] {
        let mut adapter = adapter();
        adapter.selected = Some("provider-default".into());
        select_goal_model(&mut adapter);
        let settings = command(&mut adapter, operation.clone());
        assert_eq!(settings.writes.len(), 1);
        assert_eq!(settings.writes[0]["method"], "thread/settings/update");
        assert_eq!(
            settings.writes[0]["params"],
            json!({
                "threadId":"thread-exact", "model":"explicit-goal-model", "effort":"low"
            })
        );
        assert!(published(&settings).pending);
        assert!(command_busy(&mut adapter));
        let matching_notification = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap();
        assert!(
            matching_notification.writes.is_empty(),
            "notification alone cannot start Goal work"
        );
        let applied = adapter
            .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
            .unwrap();
        assert_eq!(applied.writes.len(), 1);
        assert_eq!(applied.writes[0]["method"], "thread/goal/set");
        let expected = match operation {
            Operation::SetObjective(objective) => {
                json!({"threadId":"thread-exact","objective":objective})
            }
            Operation::Resume => json!({"threadId":"thread-exact","status":"active"}),
            _ => unreachable!(),
        };
        assert_eq!(applied.writes[0]["params"], expected);
        assert!(
            snapshot(&mut adapter).pending,
            "the same physical Goal slot spans both RPCs"
        );
        let confirmed = adapter
            .receive(json!({"id":applied.writes[0]["id"],"result":{
                "goal":raw_goal("thread-exact","Use my selected model","active")
            }}))
            .unwrap();
        assert!(!published(&confirmed).pending);
        assert_eq!(
            published(&confirmed).last_result.unwrap().outcome,
            chat::goal::Outcome::Confirmed
        );
        assert!(confirmed.writes.is_empty());
    }
}

#[test]
fn settings_ack_waits_for_actual_application_and_unspecified_effort_preserves_provider_truth() {
    for unspecified in [false, true] {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        if unspecified {
            adapter
                .command(Command::SetModel {
                    model: "explicit-goal-model".into(),
                    reasoning: None,
                })
                .unwrap();
        }
        let settings = command(&mut adapter, Operation::Resume);
        if unspecified {
            assert!(
                settings.writes[0]["params"].get("effort").is_none(),
                "an unspecified effort is not a provider reset request"
            );
        } else {
            assert_eq!(settings.writes[0]["params"]["effort"], "low");
        }
        let acknowledged = adapter
            .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
            .unwrap();
        assert!(
            acknowledged.writes.is_empty(),
            "an enqueue acknowledgement is not Core application"
        );
        assert!(snapshot(&mut adapter).pending);
        assert!(command_busy(&mut adapter));
        let foreign = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"different-thread","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap();
        assert!(foreign.writes.is_empty());
        assert!(snapshot(&mut adapter).pending);
        for thread in [None, Some(Value::Null), Some(json!(7))] {
            let mut params =
                json!({"threadSettings":{"model":"explicit-goal-model","effort":"low"}});
            if let Some(thread) = thread {
                params["threadId"] = thread;
            }
            let unbound = adapter
                .receive(json!({"method":"thread/settings/updated","params":params}))
                .unwrap();
            assert!(
                unbound.writes.is_empty(),
                "an absent or invalid thread witness cannot authorize Goal work"
            );
            assert!(snapshot(&mut adapter).pending);
            assert!(
                adapter.applied_selection.is_none(),
                "an unbound notice cannot replace unknown application proof"
            );
        }
        let applied = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap();
        assert_eq!(applied.writes.len(), 1);
        assert_eq!(applied.writes[0]["method"], "thread/goal/set");
        assert_eq!(
            applied.writes[0]["params"],
            json!({"threadId":"thread-exact","status":"active"})
        );
        assert_eq!(adapter.reasoning.as_deref(), Some("low"));
        assert!(applied.events.iter().any(|event| matches!(event, Event::Models { reasoning: Some(reasoning), .. } if reasoning == "low")),
            "the actual retained effort is published before native work");
    }
}

#[test]
fn already_applied_selection_skips_noop_settings_and_reconnect_drops_that_proof() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    let settings = command(&mut adapter, Operation::SetObjective("Original".into()));
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    let applied = adapter
        .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
        .unwrap();
    adapter
        .receive(json!({"id":applied.writes[0]["id"],"result":{
            "goal":raw_goal("thread-exact","Original","paused")
        }}))
        .unwrap();
    for reasoning in [Some("low".into()), None] {
        adapter
            .command(Command::SetModel {
                model: "explicit-goal-model".into(),
                reasoning,
            })
            .unwrap();
        let resume = command(&mut adapter, Operation::Resume);
        assert_eq!(resume.writes.len(), 1);
        assert_eq!(
            resume.writes[0]["method"], "thread/goal/set",
            "unchanged provider settings do not promise a new applied notification"
        );
        assert_eq!(adapter.reasoning.as_deref(), Some("low"));
        assert!(adapter.goal_settings.is_none());
        adapter
            .receive(json!({"id":resume.writes[0]["id"],"result":{
                "goal":raw_goal("thread-exact","Original","paused")
            }}))
            .unwrap();
    }
    drop(adapter.initialize().unwrap());
    assert!(
        adapter.applied_selection.is_none(),
        "a reconnect cannot reuse old application proof"
    );
    // A malformed attach that omitted provider model settings must refuse
    // selected Goal work before it reserves a local request or waits forever.
    adapter.ready = true;
    let current = snapshot(&mut adapter);
    let error = adapter
        .command(Command::Goal {
            thread_id: current.thread_id,
            revision: current.revision,
            operation: Operation::Resume,
        })
        .unwrap_err();
    assert!(error.contains("provider model settings are unconfirmed"));
    assert!(!snapshot(&mut adapter).pending);
}

#[test]
fn core_settings_error_retires_unsent_goal_without_replay_in_either_ack_order() {
    for error_first in [false, true] {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        let settings = command(
            &mut adapter,
            Operation::SetObjective("Keep the draft".into()),
        );
        let ack = json!({"id":settings.writes[0]["id"],"result":{}});
        if !error_first {
            assert!(adapter.receive(ack.clone()).unwrap().writes.is_empty());
        }
        let failed = adapter.receive(json!({"method":"error","params":{
            "threadId":"thread-exact","turnId":"opaque-core-settings-submission",
            "error":{"message":"Core rejected settings","codexErrorInfo":"badRequest"},"willRetry":false
        }})).unwrap();
        assert!(failed.writes.is_empty());
        if error_first {
            assert!(
                snapshot(&mut adapter).pending,
                "the original physical settings RPC still owns its reply"
            );
            assert!(adapter.receive(ack).unwrap().writes.is_empty());
        }
        let current = snapshot(&mut adapter);
        assert!(
            !current.pending,
            "a Core refusal cannot leave an unsent Goal spinner stuck"
        );
        assert_eq!(
            current.last_result.unwrap().outcome,
            chat::goal::Outcome::Uncertain
        );
        let late = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap();
        assert!(
            late.writes.is_empty(),
            "late settings cannot replay a retired Goal intent"
        );
        assert_eq!(
            command(&mut adapter, Operation::Get).writes[0]["method"],
            "thread/goal/get"
        );
    }
}

#[test]
fn missing_applied_observation_expires_only_after_ack_and_late_notice_cannot_replay() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    let settings = command(
        &mut adapter,
        Operation::SetObjective("Keep the original draft".into()),
    );
    assert!(
        adapter.next_deadline().is_none(),
        "an unresolved physical RPC has no local expiry"
    );
    assert!(
        adapter
            .on_deadline(Instant::now() + GOAL_SETTINGS_WAIT * 2)
            .unwrap()
            .writes
            .is_empty()
    );
    assert!(snapshot(&mut adapter).pending);
    let acknowledged = adapter
        .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
        .unwrap();
    assert!(acknowledged.writes.is_empty());
    let deadline = adapter
        .next_deadline()
        .expect("the now-unsent local Goal wait is bounded");
    let expired = adapter.on_deadline(deadline).unwrap();
    assert!(expired.writes.is_empty());
    assert!(!published(&expired).pending);
    assert_eq!(
        published(&expired).last_result.unwrap().outcome,
        chat::goal::Outcome::Uncertain
    );
    assert!(adapter.next_deadline().is_none());
    let late = adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    assert!(late.writes.is_empty());
    let check = command(&mut adapter, Operation::Get);
    assert_eq!(check.writes[0]["method"], "thread/goal/get");
    assert!(snapshot(&mut adapter).pending);
    adapter.on_deadline(deadline + GOAL_SETTINGS_WAIT).unwrap();
    assert!(
        snapshot(&mut adapter).pending,
        "an old deadline cannot retire a newer physical Goal RPC"
    );
}

#[test]
fn lost_settings_notice_cannot_reuse_the_previous_model_proof_after_goal_expiry() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    let settings = command(
        &mut adapter,
        Operation::SetObjective("Original request".into()),
    );
    assert!(adapter.applied_selection.is_none());
    adapter
        .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
        .unwrap();
    let deadline = adapter.next_deadline().unwrap();
    adapter.on_deadline(deadline).unwrap();
    // Core may already have applied the new model even when its notice was
    // dropped. Check only reconciles Goal state; it cannot prove the old model.
    let check = command(&mut adapter, Operation::Get);
    adapter
        .receive(json!({"id":check.writes[0]["id"],"result":{"goal":null}}))
        .unwrap();
    adapter
        .command(Command::SetModel {
            model: "provider-default".into(),
            reasoning: Some("medium".into()),
        })
        .unwrap();
    let current = snapshot(&mut adapter);
    let error = adapter
        .command(Command::Goal {
            thread_id: current.thread_id,
            revision: current.revision,
            operation: Operation::Resume,
        })
        .unwrap_err();
    assert!(error.contains("provider model settings are unconfirmed"));
    assert!(
        !snapshot(&mut adapter).pending,
        "unknown application cannot create another waiting Goal"
    );
    let late = adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    assert!(
        late.writes.is_empty(),
        "the expired original request stays retired"
    );
    let retry = command(&mut adapter, Operation::Resume);
    assert_eq!(
        retry.writes[0]["method"], "thread/settings/update",
        "the user's newly selected original model must actually be applied again"
    );
    assert_eq!(retry.writes[0]["params"]["model"], "provider-default");
}

#[test]
fn model_changing_turn_and_fast_requests_invalidate_only_changed_application_proof() {
    for fast in [false, true] {
        for already_applied in [false, true] {
            let mut adapter = adapter();
            select_goal_model(&mut adapter);
            if already_applied {
                adapter.receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
                }})).unwrap();
            }
            let request = if fast {
                adapter.fast_models.insert("explicit-goal-model".into());
                Command::SetFastMode { enabled: true }
            } else {
                Command::Prompt {
                    text: "ordinary explicit prompt".into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                }
            };
            let issued = adapter.command(request).unwrap();
            assert_eq!(issued.writes.len(), 1);
            assert_eq!(
                adapter.applied_selection.is_some(),
                already_applied,
                "only a genuinely unchanged selection can retain its actual application proof"
            );
        }
    }
}

#[test]
fn unacknowledged_model_requests_cannot_enqueue_later_conflicting_changes_out_of_order() {
    for fast in [false, true] {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        adapter
            .fast_models
            .extend(["provider-default".into(), "explicit-goal-model".into()]);
        adapter
            .command(Command::SetModel {
                model: "provider-default".into(),
                reasoning: Some("medium".into()),
            })
            .unwrap();
        let earlier = adapter
            .command(if fast {
                Command::SetFastMode { enabled: true }
            } else {
                Command::Prompt {
                    text: "unchanged selection".into(),
                    attachments: Vec::new(),
                    mode: PromptMode::Send,
                }
            })
            .unwrap();
        assert_eq!(earlier.writes[0]["params"]["model"], "provider-default");
        // An outstanding compatible override does not invalidate actual A.
        let unchanged = command(&mut adapter, Operation::Resume);
        assert_eq!(unchanged.writes[0]["method"], "thread/goal/set");
        adapter
            .receive(json!({"id":unchanged.writes[0]["id"],"result":{
                "goal":raw_goal("thread-exact","Original","paused")
            }}))
            .unwrap();
        adapter
            .command(Command::SetModel {
                model: "explicit-goal-model".into(),
                reasoning: Some("low".into()),
            })
            .unwrap();
        let before = snapshot(&mut adapter);
        let before_id = adapter.next_id;
        for request in [
            Command::SetFastMode { enabled: false },
            Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::Resume,
            },
        ] {
            assert!(adapter.command(request).is_err());
            assert_eq!(
                adapter.next_id, before_id,
                "conflicting request was not allocated"
            );
            assert_eq!(snapshot(&mut adapter), before);
            assert!(adapter.unconfirmed_selection.is_none());
        }
        adapter
            .receive(json!({"id":earlier.writes[0]["id"],"result":if fast {
            json!({})
        } else { json!({"turn":{"id":"unchanged-selection-turn"}}) }}))
            .unwrap();
        let next = command(&mut adapter, Operation::Resume);
        assert_eq!(next.writes[0]["method"], "thread/settings/update");
        assert_eq!(next.writes[0]["params"]["model"], "explicit-goal-model");
    }
}

#[test]
fn older_settings_notices_cannot_restore_proof_while_turn_fast_or_goal_changes_can_execute() {
    for origin in 0..3 {
        for ack_first in [false, true] {
            let mut adapter = adapter();
            select_goal_model(&mut adapter);
            adapter
                .fast_models
                .extend(["provider-default".into(), "explicit-goal-model".into()]);
            adapter
                .command(Command::SetModel {
                    model: "provider-default".into(),
                    reasoning: Some("medium".into()),
                })
                .unwrap();
            // The same-selection Fast operation may send its actual A
            // snapshot after its ACK and after a newer B request is enqueued.
            let older = adapter
                .command(Command::SetFastMode { enabled: true })
                .unwrap();
            adapter
                .receive(json!({"id":older.writes[0]["id"],"result":{}}))
                .unwrap();
            adapter
                .command(Command::SetModel {
                    model: "explicit-goal-model".into(),
                    reasoning: Some("low".into()),
                })
                .unwrap();
            let newer = match origin {
                0 => adapter
                    .command(Command::Prompt {
                        text: "ordinary selected-model prompt".into(),
                        attachments: Vec::new(),
                        mode: PromptMode::Send,
                    })
                    .unwrap(),
                1 => adapter
                    .command(Command::SetFastMode { enabled: false })
                    .unwrap(),
                _ => command(&mut adapter, Operation::Resume),
            };
            let ack = json!({"id":newer.writes[0]["id"],"result":if origin == 0 {
                json!({"turn":{"id":"newer-model-turn"}})
            } else { json!({}) }});
            if ack_first {
                assert!(adapter.receive(ack.clone()).unwrap().writes.is_empty());
            }
            let stale = adapter
                .receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact","threadSettings":{
                        "model":"provider-default","effort":"medium","serviceTier":"fast"
                    }
                }}))
                .unwrap();
            assert!(stale.writes.is_empty());
            if !ack_first {
                assert!(adapter.receive(ack).unwrap().writes.is_empty());
            }
            assert!(
                adapter.applied_selection.is_none(),
                "origin {origin}, ACK-first {ack_first}"
            );
            let check = command(&mut adapter, Operation::Get);
            adapter
                .receive(json!({"id":check.writes[0]["id"],"result":{"goal":null}}))
                .unwrap();
            adapter
                .command(Command::SetModel {
                    model: "provider-default".into(),
                    reasoning: Some("medium".into()),
                })
                .unwrap();
            let before = snapshot(&mut adapter);
            let error = adapter
                .command(Command::Goal {
                    thread_id: before.thread_id.clone(),
                    revision: before.revision,
                    operation: Operation::Resume,
                })
                .unwrap_err();
            assert!(error.contains("provider model settings are unconfirmed"));
            assert_eq!(
                snapshot(&mut adapter),
                before,
                "refusal reserves no Goal intent"
            );
            let settled = adapter.receive(json!({"method":"thread/settings/updated","params":{
                "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
            }})).unwrap();
            assert!(
                settled.writes.is_empty(),
                "settlement cannot replay an old Goal"
            );
            let retry = command(&mut adapter, Operation::Resume);
            assert_eq!(retry.writes[0]["method"], "thread/settings/update");
            assert_eq!(retry.writes[0]["params"]["model"], "provider-default");
        }
    }
}

#[test]
fn local_selection_custody_survives_ack_expiry_and_same_server_reattachment() {
    for stale_before_expiry in [false, true] {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        let issued = command(&mut adapter, Operation::Resume);
        adapter
            .receive(json!({"id":issued.writes[0]["id"],"result":{}}))
            .unwrap();
        let deadline = adapter.next_deadline().unwrap();
        let stale = json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"provider-default","effort":"medium"}
        }});
        if stale_before_expiry {
            assert!(adapter.receive(stale.clone()).unwrap().writes.is_empty());
        }
        assert!(adapter.on_deadline(deadline).unwrap().writes.is_empty());
        if !stale_before_expiry {
            assert!(adapter.receive(stale).unwrap().writes.is_empty());
        }
        assert!(!snapshot(&mut adapter).pending);
        let reattached = attach(&mut adapter);
        let read = reattached
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/goal/get")
            .unwrap();
        adapter
            .receive(json!({"id":read["id"],"result":{"goal":null}}))
            .unwrap();
        assert!(
            adapter.applied_selection.is_none(),
            "resume snapshot A is no Core queue flush"
        );
        adapter
            .command(Command::SetModel {
                model: "provider-default".into(),
                reasoning: Some("medium".into()),
            })
            .unwrap();
        let current = snapshot(&mut adapter);
        assert!(
            adapter
                .command(Command::Goal {
                    thread_id: current.thread_id,
                    revision: current.revision,
                    operation: Operation::Resume,
                })
                .unwrap_err()
                .contains("provider model settings are unconfirmed")
        );
        assert!(adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap().writes.is_empty());
        let retry = command(&mut adapter, Operation::Resume);
        assert_eq!(retry.writes[0]["method"], "thread/settings/update");
    }
}

#[test]
fn conflicting_model_changes_wait_for_positive_application_without_aba_shortcuts() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    adapter
        .fast_models
        .extend(["provider-default".into(), "explicit-goal-model".into()]);
    let changed = adapter
        .command(Command::SetFastMode { enabled: true })
        .unwrap();
    adapter
        .receive(json!({"id":changed.writes[0]["id"],"result":{}}))
        .unwrap();
    adapter
        .command(Command::SetModel {
            model: "provider-default".into(),
            reasoning: Some("medium".into()),
        })
        .unwrap();
    let before = snapshot(&mut adapter);
    let before_id = adapter.next_id;
    for request in [
        Command::Prompt {
            text: "selected-model prompt".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        },
        Command::SetFastMode { enabled: false },
        Command::Goal {
            thread_id: before.thread_id.clone(),
            revision: before.revision,
            operation: Operation::Resume,
        },
    ] {
        assert!(adapter.command(request).is_err());
        assert_eq!(
            adapter.next_id, before_id,
            "conflicting request was not allocated"
        );
        assert!(adapter.pending.is_empty());
        assert_eq!(
            snapshot(&mut adapter),
            before,
            "no Goal intent was reserved"
        );
    }
    for (model, effort, confirmed) in [
        ("provider-default", "medium", false),
        ("explicit-goal-model", "low", true),
    ] {
        assert!(
            adapter
                .receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact","threadSettings":{"model":model,"effort":effort}
                }}))
                .unwrap()
                .writes
                .is_empty()
        );
        assert_eq!(adapter.unconfirmed_selection.is_none(), confirmed);
    }
    let next = adapter
        .command(Command::SetFastMode { enabled: false })
        .unwrap();
    assert_eq!(next.writes[0]["params"]["model"], "provider-default");
    adapter
        .receive(json!({"id":next.writes[0]["id"],"result":{}}))
        .unwrap();
    // Another old B snapshot cannot settle the user's new A override either.
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    assert!(adapter.applied_selection.is_none());
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"provider-default","effort":"medium"}
    }})).unwrap();
    let resume = command(&mut adapter, Operation::Resume);
    assert_eq!(resume.writes[0]["method"], "thread/goal/set");
}

#[test]
fn compatible_model_changes_share_one_bounded_application_fence() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    for enabled in [true, false] {
        adapter.fast_models.insert("explicit-goal-model".into());
        let changed = adapter.command(Command::SetFastMode { enabled }).unwrap();
        adapter
            .receive(json!({"id":changed.writes[0]["id"],"result":{}}))
            .unwrap();
        assert_eq!(
            adapter.unconfirmed_selection.as_ref().unwrap().target,
            (Some("explicit-goal-model".into()), Some("low".into()))
        );
        assert!(adapter.applied_selection.is_none());
    }
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    assert!(adapter.unconfirmed_selection.is_none());
    assert!(adapter.selection_is_applied());
    let resume = command(&mut adapter, Operation::Resume);
    assert_eq!(resume.writes[0]["method"], "thread/goal/set");
}

#[test]
fn an_observed_goal_selection_still_preserves_its_unacknowledged_physical_order() {
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    let settings = command(&mut adapter, Operation::Resume);
    adapter.receive(json!({"method":"thread/settings/updated","params":{
        "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
    }})).unwrap();
    adapter
        .fast_models
        .extend(["provider-default".into(), "explicit-goal-model".into()]);
    adapter
        .command(Command::SetModel {
            model: "provider-default".into(),
            reasoning: Some("medium".into()),
        })
        .unwrap();
    let before_id = adapter.next_id;
    for request in [
        Command::Prompt {
            text: "conflicting selection".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        },
        Command::SetFastMode { enabled: true },
    ] {
        assert!(adapter.command(request).is_err());
        assert_eq!(adapter.next_id, before_id);
        assert!(snapshot(&mut adapter).pending);
    }
    adapter
        .command(Command::SetModel {
            model: "explicit-goal-model".into(),
            reasoning: Some("low".into()),
        })
        .unwrap();
    let applied = adapter
        .receive(json!({"id":settings.writes[0]["id"],"result":{}}))
        .unwrap();
    assert_eq!(applied.writes.len(), 1);
    assert_eq!(applied.writes[0]["method"], "thread/goal/set");
}

#[test]
fn only_a_sole_settings_rpc_pre_enqueue_rejection_releases_uncertain_selection_custody() {
    for fast in [false, true] {
        for (error, rejected) in [
            (
                json!({"code":-32600,"message":"invalid model override"}),
                true,
            ),
            (json!({"code":-32601,"message":"method not found"}), true),
            (json!({"code":-32602,"message":"invalid params"}), true),
            (json!({"code":-32603,"message":"internal failure"}), false),
            (json!({"code":"-32600","message":"malformed code"}), false),
            (json!({"code":-32600}), false),
        ] {
            let mut adapter = adapter();
            select_goal_model(&mut adapter);
            let issued = if fast {
                adapter.fast_models.insert("explicit-goal-model".into());
                adapter
                    .command(Command::SetFastMode { enabled: true })
                    .unwrap()
            } else {
                command(&mut adapter, Operation::Resume)
            };
            let failed = adapter
                .receive(json!({"id":issued.writes[0]["id"],"error":error}))
                .unwrap();
            assert!(failed.writes.is_empty());
            assert_eq!(adapter.unconfirmed_selection.is_none(), rejected);
            assert!(
                adapter.applied_selection.is_none(),
                "rejection cannot resurrect old A proof"
            );
            let reattached = attach(&mut adapter);
            let read = reattached
                .writes
                .iter()
                .find(|frame| frame["method"] == "thread/goal/get")
                .unwrap();
            adapter
                .receive(json!({"id":read["id"],"result":{"goal":null}}))
                .unwrap();
            assert_eq!(adapter.applied_selection.is_some(), rejected);
            adapter
                .command(Command::SetModel {
                    model: "provider-default".into(),
                    reasoning: Some("medium".into()),
                })
                .unwrap();
            let current = snapshot(&mut adapter);
            let retry = adapter.command(Command::Goal {
                thread_id: current.thread_id,
                revision: current.revision,
                operation: Operation::Resume,
            });
            if rejected {
                assert_eq!(retry.unwrap().writes[0]["method"], "thread/goal/set");
            } else {
                assert!(
                    retry
                        .unwrap_err()
                        .contains("provider model settings are unconfirmed")
                );
                assert!(!snapshot(&mut adapter).pending);
            }
        }
    }
    let mut adapter = adapter();
    select_goal_model(&mut adapter);
    let issued = command(&mut adapter, Operation::Resume);
    adapter
        .receive(json!({"id":issued.writes[0]["id"],"result":{},
        "error":{"code":-32600,"message":"ambiguous malformed response"}}))
        .unwrap();
    assert!(
        adapter.unconfirmed_selection.is_some(),
        "a response with both result and error proves no rejection boundary"
    );
}

#[test]
fn uncertain_selection_recovers_only_through_a_new_explicit_same_target_goal() {
    for failure in 0..3 {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        let old = command(
            &mut adapter,
            Operation::SetObjective("Never replay this objective".into()),
        );
        if failure == 2 {
            adapter
                .receive(json!({"id":old.writes[0]["id"],"error":{
                    "code":-32603,"message":"unknown internal failure"
                }}))
                .unwrap();
        } else {
            adapter
                .receive(json!({"id":old.writes[0]["id"],"result":{}}))
                .unwrap();
            if failure == 0 {
                adapter.receive(json!({"method":"error","params":{
                    "threadId":"thread-exact","turnId":"opaque-core-submission",
                    "error":{"message":"invalid thread settings override","codexErrorInfo":"badRequest"},
                    "willRetry":false
                }})).unwrap();
            } else {
                adapter
                    .on_deadline(adapter.next_deadline().unwrap())
                    .unwrap();
            }
        }
        assert!(!snapshot(&mut adapter).pending);
        assert!(adapter.unconfirmed_selection.is_some());
        let check = command(&mut adapter, Operation::Get);
        adapter
            .receive(json!({"id":check.writes[0]["id"],"result":{"goal":null}}))
            .unwrap();
        let rejected_retry = command(
            &mut adapter,
            Operation::SetObjective("Rejected replacement".into()),
        );
        assert_eq!(rejected_retry.writes[0]["method"], "thread/settings/update");
        assert!(
            adapter
                .receive(json!({"id":rejected_retry.writes[0]["id"],"error":{
                    "code":-32600,"message":"invalid replacement settings"
                }}))
                .unwrap()
                .writes
                .is_empty()
        );
        assert!(
            adapter.unconfirmed_selection.is_some(),
            "a later rejection cannot release an earlier effect"
        );
        let check = command(&mut adapter, Operation::Get);
        adapter
            .receive(json!({"id":check.writes[0]["id"],"result":{"goal":null}}))
            .unwrap();
        let replacement = command(
            &mut adapter,
            Operation::SetObjective("Only this new objective".into()),
        );
        assert_eq!(replacement.writes[0]["method"], "thread/settings/update");
        assert!(
            adapter
                .receive(json!({"id":replacement.writes[0]["id"],"result":{}}))
                .unwrap()
                .writes
                .is_empty()
        );
        let applied = adapter.receive(json!({"method":"thread/settings/updated","params":{
            "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"low"}
        }})).unwrap();
        assert_eq!(applied.writes.len(), 1);
        assert_eq!(applied.writes[0]["method"], "thread/goal/set");
        assert_eq!(
            applied.writes[0]["params"]["objective"],
            "Only this new objective"
        );
    }
}

#[test]
fn failed_or_superseded_goal_settings_never_send_or_retry_a_goal_mutation() {
    for case in 0..6 {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        let settings = command(
            &mut adapter,
            Operation::SetObjective("Keep this unsent".into()),
        );
        match case {
            2 => {
                adapter.receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact","threadSettings":{"model":"different-model","effort":"low"}
                }})).unwrap();
            }
            3 => {
                adapter.receive(json!({"method":"thread/settings/updated","params":{
                    "threadId":"thread-exact","threadSettings":{"model":"explicit-goal-model","effort":"high"}
                }})).unwrap();
            }
            4 => {
                adapter.receive(json!({"method":"thread/goal/updated","params":{
                    "threadId":"thread-exact","goal":raw_goal("thread-exact","New external goal","paused")
                }})).unwrap();
            }
            5 => {
                drop(adapter.initialize().unwrap());
            }
            _ => {}
        }
        let response = match case {
            0 => {
                json!({"id":settings.writes[0]["id"],"error":{"code":-32601,"message":"settings unavailable"}})
            }
            1 => json!({"id":settings.writes[0]["id"],"result":null}),
            _ => json!({"id":settings.writes[0]["id"],"result":{}}),
        };
        let refused = adapter.receive(response).unwrap();
        assert!(
            refused.writes.is_empty(),
            "case {case} must not start or retry Goal work"
        );
        let observed = published(&refused);
        assert!(!observed.pending);
        assert_ne!(
            observed.support,
            Support::Unsupported,
            "settings support is not Goal support"
        );
        if case == 4 {
            assert!(
                matches!(&observed.read, ReadState::Known(Some(goal)) if goal.objective == "New external goal")
            );
        }
        if case == 5 {
            assert!(
                observed.last_result.is_none(),
                "reconnect cannot complete the old owner intent"
            );
            assert!(
                !refused
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Error { .. })),
                "the old settings callback cannot report an error into the new binding"
            );
        } else {
            assert_eq!(
                observed.last_result.unwrap().outcome,
                chat::goal::Outcome::Uncertain
            );
        }
    }
}

#[test]
fn goal_read_pause_and_clear_do_not_apply_staged_execution_settings() {
    for (operation, method) in [
        (Operation::Get, "thread/goal/get"),
        (Operation::Pause, "thread/goal/set"),
        (Operation::Clear, "thread/goal/clear"),
    ] {
        let mut adapter = adapter();
        select_goal_model(&mut adapter);
        let issued = command(&mut adapter, operation);
        assert_eq!(issued.writes.len(), 1);
        assert_eq!(issued.writes[0]["method"], method);
        assert!(issued.writes[0]["params"].get("model").is_none());
        assert!(issued.writes[0]["params"].get("effort").is_none());
    }
}

fn attach(adapter: &mut Codex) -> Step {
    let initialize = adapter.initialize().unwrap();
    let initialized = adapter
        .receive(json!({"id":initialize.writes[0]["id"],
        "result":{"codexHome":adapter.expected_home,"userAgent":"fixture"}}))
        .unwrap();
    let resume = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume")
        .unwrap();
    assert_eq!(resume["params"]["threadId"], "thread-exact");
    let attached = adapter
        .receive(json!({"id":resume["id"],"result":{
        "thread":{"id":"thread-exact","canAcceptDirectInput":true,"status":{"type":"idle"}},
        "model":"provider-default","reasoningEffort":"medium",
        "initialTurnsPage":{"data":[],"nextCursor":null}}}))
        .unwrap();
    let skills = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    adapter
        .receive(json!({"id":skills["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(adapter.ready);
    attached
}

#[test]
fn goal_work_respects_direct_input_while_exact_read_and_stop_remain_usable() {
    let mut adapter = adapter();
    adapter.direct_input = false;
    for operation in [
        Operation::SetObjective("Do more work".into()),
        Operation::Resume,
    ] {
        let before = snapshot(&mut adapter);
        assert!(
            adapter
                .command(Command::Goal {
                    thread_id: before.thread_id,
                    revision: before.revision,
                    operation,
                })
                .unwrap_err()
                .contains("does not accept new goal work")
        );
        assert!(adapter.pending.is_empty());
        assert!(!snapshot(&mut adapter).pending);
    }
    for (operation, reply) in [
        (
            Operation::Get,
            json!({"goal":raw_goal("thread-exact","Original objective","active")}),
        ),
        (
            Operation::Pause,
            json!({"goal":raw_goal("thread-exact","Original objective","paused")}),
        ),
        (Operation::Clear, json!({"cleared":true})),
    ] {
        let issued = command(&mut adapter, operation);
        assert_eq!(issued.writes.len(), 1);
        assert_ne!(issued.writes[0]["method"], "turn/start");
        let result = adapter
            .receive(json!({"id":issued.writes[0]["id"],"result":reply}))
            .unwrap();
        assert!(!published(&result).pending);
    }
}

#[test]
fn native_goal_commands_correlate_exact_thread_and_publish_only_actual_responses() {
    let mut adapter = adapter();
    for (operation, method, reply) in [
        (Operation::Get, "thread/goal/get", json!({"goal":null})),
        (
            Operation::SetObjective("Finish 🦀 safely".into()),
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","active")}),
        ),
        (
            Operation::Pause,
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","paused")}),
        ),
        (
            Operation::Resume,
            "thread/goal/set",
            json!({"goal":raw_goal("thread-exact","Finish 🦀 safely","complete")}),
        ),
        (
            Operation::Clear,
            "thread/goal/clear",
            json!({"cleared":true}),
        ),
    ] {
        let before = snapshot(&mut adapter);
        let issued = command(&mut adapter, operation.clone());
        assert_eq!(issued.writes.len(), 1);
        let frame = &issued.writes[0];
        assert_eq!(frame["method"], method);
        assert_eq!(frame["params"]["threadId"], "thread-exact");
        assert!(frame["params"].get("tokenBudget").is_none());
        if !matches!(operation, Operation::SetObjective(_)) {
            assert!(frame["params"].get("objective").is_none());
        }
        assert_eq!(
            published(&issued).read,
            before.read,
            "dispatch is not confirmation"
        );
        assert!(published(&issued).pending);
        assert!(command_busy(&mut adapter));
        let received = adapter
            .receive(json!({"id":frame["id"],"result":reply}))
            .unwrap();
        let actual = published(&received);
        assert!(!actual.pending);
        assert!(!actual.uncertain);
        assert_eq!(actual.support, Support::Supported);
        if operation == Operation::Resume {
            assert!(
                matches!(&actual.read, ReadState::Known(Some(goal)) if goal.status == Status::Complete)
            );
        }
        assert!(
            received.writes.is_empty(),
            "no automatic turn, goal retry or prompt"
        );
    }
    assert_eq!(snapshot(&mut adapter).read, ReadState::Known(None));
}

fn command_busy(adapter: &mut Codex) -> bool {
    let current = snapshot(adapter);
    adapter
        .command(Command::Goal {
            thread_id: current.thread_id,
            revision: current.revision,
            operation: Operation::Get,
        })
        .is_err()
}

#[test]
fn notification_wins_over_early_reply_without_releasing_physical_slot_or_newer_request() {
    let mut adapter = adapter();
    let issued = command(&mut adapter, Operation::SetObjective("original".into()));
    let observed = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"thread-exact","goal":raw_goal("thread-exact","newer","blocked")}}))
        .unwrap();
    assert!(published(&observed).pending);
    assert!(command_busy(&mut adapter));
    let released = adapter
        .receive(json!({"id":issued.writes[0]["id"],"result":{
        "goal":raw_goal("thread-exact","original","active")}}))
        .unwrap();
    assert!(
        matches!(&published(&released).read, ReadState::Known(Some(goal)) if goal.objective == "newer")
    );
    assert!(!published(&released).pending);
    let next = command(&mut adapter, Operation::Clear);
    assert!(
        adapter
            .receive(json!({"id":issued.writes[0]["id"],"result":{"goal":null}}))
            .is_err()
    );
    assert!(
        snapshot(&mut adapter).pending,
        "duplicate old reply cannot release successor"
    );
    let cleared = adapter
        .receive(json!({"method":"thread/goal/cleared","params":{"threadId":"thread-exact"}}))
        .unwrap();
    assert_eq!(published(&cleared).read, ReadState::Known(None));
    assert!(published(&cleared).pending);
    let stale = adapter
        .receive(json!({"id":next.writes[0]["id"],"result":{"cleared":false}}))
        .unwrap();
    assert_eq!(published(&stale).read, ReadState::Known(None));
    assert!(!published(&stale).uncertain);
    assert!(!published(&stale).pending);
}

#[test]
fn queued_controls_require_current_observation_even_after_response_only_replacement() {
    let mut adapter = adapter();
    let get = command(&mut adapter, Operation::Get);
    adapter.receive(json!({"id":get.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","A","paused")}})).unwrap();
    let old = snapshot(&mut adapter);
    let set = command(&mut adapter, Operation::SetObjective("B".into()));
    adapter.receive(json!({"id":set.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","B","active")}})).unwrap();
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: old.thread_id,
                revision: old.revision,
                operation: Operation::Clear
            })
            .is_err()
    );
    let current = snapshot(&mut adapter);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: "foreign".into(),
                revision: current.revision,
                operation: Operation::Clear
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), current);
    assert_eq!(
        command(&mut adapter, Operation::Clear).writes[0]["method"],
        "thread/goal/clear"
    );
}

#[test]
fn malformed_current_observations_invalidate_cache_but_foreign_threads_do_not() {
    let mut adapter = adapter();
    let read = command(&mut adapter, Operation::Get);
    adapter.receive(json!({"id":read.writes[0]["id"],"result":{"goal":raw_goal("thread-exact","A","active")}})).unwrap();
    let before = snapshot(&mut adapter);
    let foreign = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"other","goal":raw_goal("other","foreign","complete")}}))
        .unwrap();
    assert!(foreign.events.is_empty());
    assert_eq!(snapshot(&mut adapter), before);
    let pending = command(&mut adapter, Operation::Get);
    let invalid = adapter
        .receive(json!({"method":"thread/goal/updated","params":{
        "threadId":"thread-exact","goal":raw_goal("wrong-inner","foreign","complete")}}))
        .unwrap();
    assert_eq!(published(&invalid).read, ReadState::Unknown);
    assert!(published(&invalid).pending);
    assert!(
        invalid
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: false, .. }))
    );
    let late = adapter
        .receive(json!({"id":pending.writes[0]["id"],"result":{"goal":null}}))
        .unwrap();
    assert_eq!(published(&late).read, ReadState::Unknown);
    assert!(!published(&late).pending);
    let retry = command(&mut adapter, Operation::Get);
    let malformed = adapter
        .receive(json!({"id":retry.writes[0]["id"],"result":{
        "goal":raw_goal("other","foreign","complete")}}))
        .unwrap();
    assert!(published(&malformed).uncertain);
    assert_eq!(published(&malformed).read, ReadState::Unknown);
    assert!(!published(&malformed).pending);
    assert!(malformed.writes.is_empty());
}

#[test]
fn attach_and_reconnect_read_optional_goal_without_blocking_unsupported_chat() {
    let mut adapter = adapter();
    let attached = attach(&mut adapter);
    let read = attached
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/goal/get")
        .unwrap();
    assert!(published(&attached).pending);
    assert!(attached.writes.iter().all(|frame| !matches!(
        frame["method"].as_str(),
        Some("turn/start" | "thread/goal/set" | "thread/goal/clear")
    )));
    let unavailable = adapter
        .receive(json!({"id":read["id"],"error":{"code":-32601,"message":"method not found"}}))
        .unwrap();
    assert_eq!(published(&unavailable).support, Support::Unsupported);
    assert!(!published(&unavailable).uncertain);
    assert!(
        unavailable
            .events
            .iter()
            .all(|event| !matches!(event, Event::Error { fatal: true, .. }))
    );
    assert!(adapter.ready);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: "thread-exact".into(),
                revision: published(&unavailable).revision,
                operation: Operation::SetObjective("No fallback".into())
            })
            .is_err()
    );
    let prompt = adapter
        .command(Command::Prompt {
            text: "ordinary chat".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    assert_eq!(prompt.writes[0]["method"], "turn/start");
    // A fresh handshake may be a newer app-server. Check, never replay mutations.
    let reattached = attach(&mut adapter);
    assert_eq!(
        reattached
            .writes
            .iter()
            .filter(|frame| frame["method"] == "thread/goal/get")
            .count(),
        1
    );
    assert!(reattached.writes.iter().all(|frame| !matches!(
        frame["method"].as_str(),
        Some("thread/goal/set" | "thread/goal/clear")
    )));
}

#[test]
fn reconnect_keeps_unresolved_goal_slot_and_never_replays_a_mutation() {
    let mut adapter = adapter();
    let issued = command(&mut adapter, Operation::Pause);
    let attached = attach(&mut adapter);
    assert!(published(&attached).pending);
    assert_eq!(published(&attached).read, ReadState::Unknown);
    assert!(attached.writes.iter().all(|frame| {
        !frame["method"]
            .as_str()
            .unwrap()
            .starts_with("thread/goal/")
    }));
    assert!(command_busy(&mut adapter));
    let resolved = adapter
        .receive(json!({"id":issued.writes[0]["id"],"error":{"code":-32000,"message":"unknown"}}))
        .unwrap();
    assert!(!published(&resolved).pending);
    assert_eq!(published(&resolved).read, ReadState::Unknown);
    assert!(resolved.writes.is_empty());
    assert_eq!(
        command(&mut adapter, Operation::Get).writes[0]["method"],
        "thread/goal/get"
    );
}

#[test]
fn failed_request_admission_and_invalid_direct_objective_do_not_reserve_a_slot() {
    let mut adapter = adapter();
    let before = snapshot(&mut adapter);
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::SetObjective("🦀".repeat(4_001))
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
    for id in 0..MAX_PENDING {
        adapter.pending.insert(id.to_string(), Pending::Other);
    }
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::Get
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
    adapter.pending.clear();
    adapter.next_id = u64::MAX;
    assert!(
        adapter
            .command(Command::Goal {
                thread_id: before.thread_id.clone(),
                revision: before.revision,
                operation: Operation::Get
            })
            .is_err()
    );
    assert_eq!(snapshot(&mut adapter), before);
}

/// The runtime cannot authenticate initialize's server identity until the
/// initialize response is decoded. Unknown advisory must still allow this
/// metadata request, without loading or resuming the retained thread.
fn begin_legacy_restore(adapter: &mut Codex) -> Step {
    adapter.set_goal_resume_admission(GoalResumeAdmission::Unavailable);
    let initialize = adapter.initialize().unwrap();
    let step = adapter
        .receive(json!({"id":initialize.writes[0]["id"],"result":{
            "codexHome":adapter.expected_home,"userAgent":"fixture"}}))
        .unwrap();
    assert!(adapter.server_identity().is_some());
    assert!(!adapter.ready);
    assert!(step.writes.iter().all(|frame| !matches!(
        frame["method"].as_str(),
        Some("thread/resume" | "thread/start" | "turn/start")
    )));
    assert_eq!(
        step.writes
            .iter()
            .filter(|frame| frame["method"] == "thread/goal/get")
            .count(),
        1
    );
    step
}

#[test]
fn legacy_restore_reads_before_resume_and_requires_positive_inactive_native_truth() {
    for status in [
        None,
        Some("paused"),
        Some("complete"),
        Some("active"),
        Some("blocked"),
        Some("usageLimited"),
        Some("budgetLimited"),
    ] {
        let mut adapter = adapter();
        let initialized = begin_legacy_restore(&mut adapter);
        let get = initialized
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/goal/get")
            .unwrap();
        // Real runtime refresh occurs before this receive, after identity was
        // established by the preceding initialize response.
        adapter.set_goal_resume_admission(GoalResumeAdmission::InactiveOnly);
        let result = adapter
            .receive(json!({"id":get["id"],"result":{
            "goal":status.map(|status| raw_goal("thread-exact", "Original goal", status))}}))
            .unwrap();
        let inactive = matches!(status, None | Some("paused" | "complete"));
        assert_eq!(
            result
                .writes
                .iter()
                .any(|frame| frame["method"] == "thread/resume"),
            inactive,
            "{status:?}"
        );
        assert_eq!(adapter.goal_resume_pending, !inactive);
        assert!(!adapter.ready, "metadata is not an attached native session");
        assert!(result.writes.iter().all(|frame| !matches!(
            frame["method"].as_str(),
            Some("turn/start" | "thread/goal/set")
        )));
        assert!(
            adapter
                .command(Command::Prompt {
                    text: "retain ordinary input".into(),
                    attachments: vec![],
                    mode: PromptMode::Send
                })
                .is_err()
        );
    }
}

#[test]
fn blocked_legacy_restore_keeps_native_pause_and_clear_without_claiming_ready() {
    for clear in [false, true] {
        let mut adapter = adapter();
        let initialized = begin_legacy_restore(&mut adapter);
        let get = initialized
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/goal/get")
            .unwrap();
        adapter.set_goal_resume_admission(GoalResumeAdmission::InactiveOnly);
        let active = adapter
            .receive(json!({"id":get["id"],"result":{
            "goal":raw_goal("thread-exact", "Original goal", "active")}}))
            .unwrap();
        assert!(active.writes.is_empty());
        assert!(adapter.goal_resume_pending && !adapter.ready);
        let before = snapshot(&mut adapter);
        assert!(
            adapter
                .command(Command::Goal {
                    thread_id: before.thread_id,
                    revision: before.revision,
                    operation: Operation::Resume
                })
                .is_err()
        );
        let stop = command(
            &mut adapter,
            if clear {
                Operation::Clear
            } else {
                Operation::Pause
            },
        );
        assert_eq!(
            stop.writes[0]["method"],
            if clear {
                "thread/goal/clear"
            } else {
                "thread/goal/set"
            }
        );
        assert!(stop.writes[0]["params"].get("objective").is_none());
        let reply = if clear {
            json!({"cleared":true})
        } else {
            json!({"goal":raw_goal("thread-exact", "Original goal", "paused")})
        };
        let stopped = adapter
            .receive(json!({"id":stop.writes[0]["id"],"result":reply}))
            .unwrap();
        assert_eq!(stopped.writes.len(), 1);
        assert_eq!(stopped.writes[0]["method"], "thread/resume");
        assert_eq!(stopped.writes[0]["params"]["threadId"], "thread-exact");
        assert!(!adapter.ready);
    }
}

#[test]
fn unavailable_scope_unknown_or_unsupported_goal_never_falls_back_to_legacy_resume() {
    for reply in [
        json!({"result":{"goal":null}}),
        json!({"error":{"code":-32601,"message":"not implemented"}}),
        json!({"error":{"code":-32000,"message":"temporarily unavailable"}}),
        json!({"result":{"goal":raw_goal("foreign-thread", "Wrong source", "paused")}}),
    ] {
        let mut adapter = adapter();
        let initialized = begin_legacy_restore(&mut adapter);
        let get = initialized
            .writes
            .iter()
            .find(|frame| frame["method"] == "thread/goal/get")
            .unwrap();
        // Even a positive None cannot restore failed physical authority.
        adapter.set_goal_resume_admission(
            if reply["result"]["goal"].is_null() && reply.get("result").is_some() {
                GoalResumeAdmission::Unavailable
            } else {
                GoalResumeAdmission::InactiveOnly
            },
        );
        let mut frame = reply;
        frame["id"] = get["id"].clone();
        let blocked = adapter.receive(frame).unwrap();
        assert!(blocked.writes.is_empty());
        assert!(adapter.goal_resume_pending && !adapter.ready);
        assert!(!published(&blocked).pending);
        assert!(
            blocked.events.iter().all(|event| !matches!(
                event,
                Event::Error { fatal: true, .. } | Event::Ready(true)
            ))
        );
    }
}

#[test]
fn disabled_native_goal_errors_preserve_uncertainty_and_explain_only_the_exact_signature() {
    const DISABLED: &str = "Goals are disabled in this Codex connection. Enable Goals in the selected Codex configuration before starting a new connection. Your input is kept.";
    for operation in [
        Operation::Get,
        Operation::SetObjective("Original objective kept".into()),
        Operation::Clear,
    ] {
        let mut adapter = adapter();
        let get = command(&mut adapter, Operation::Get);
        adapter
            .receive(json!({"id":get.writes[0]["id"], "result":{
                "goal":raw_goal("thread-exact", "Original observed goal", "paused")
            }}))
            .unwrap();
        let before = snapshot(&mut adapter);
        let issued = command(&mut adapter, operation.clone());
        assert_eq!(issued.writes.len(), 1);
        assert_eq!(issued.writes[0]["params"]["threadId"], "thread-exact");
        assert_eq!(
            issued.writes[0]["method"],
            match &operation {
                Operation::Get => "thread/goal/get",
                Operation::SetObjective(_) => "thread/goal/set",
                Operation::Clear => "thread/goal/clear",
                _ => unreachable!(),
            }
        );
        if let Operation::SetObjective(objective) = &operation {
            assert_eq!(issued.writes[0]["params"]["objective"], objective.as_str());
        }
        let resolved = adapter
            .receive(json!({"id":issued.writes[0]["id"], "error":{
                "code":-32600,"message":"goals feature is disabled","data":null
            }}))
            .unwrap();
        let actual = published(&resolved);
        assert!(!actual.pending && actual.uncertain);
        assert_eq!(actual.read, before.read);
        assert_eq!(actual.support, before.support);
        assert_eq!(
            actual.last_result,
            Some(chat::goal::Completion {
                revision: before.revision,
                outcome: chat::goal::Outcome::Uncertain,
            })
        );
        assert!(
            resolved.writes.is_empty(),
            "no retry, prompt or config mutation"
        );
        assert!(adapter.ready);
        assert!(adapter.pending.is_empty());
        let errors = resolved
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Error { message, fatal } => Some((message.as_str(), *fatal)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(errors, vec![(DISABLED, false)]);
        // The original RPC really resolved. Only a new explicit Check can
        // claim the now-free slot; the diagnostic does not enqueue one.
        let check = command(&mut adapter, Operation::Get);
        assert_eq!(check.writes.len(), 1);
        assert_eq!(check.writes[0]["method"], "thread/goal/get");
    }
    for (code, message, unsupported) in [
        (-32600, "unrelated invalid request", false),
        (-32600, "Goals feature is disabled", false),
        (-32000, "goals feature is disabled", false),
        (-32601, "goals feature is disabled", true),
    ] {
        let mut adapter = adapter();
        let issued = command(&mut adapter, Operation::Get);
        let resolved = adapter
            .receive(json!({"id":issued.writes[0]["id"], "error":{
                "code":code,"message":message
            }}))
            .unwrap();
        let actual = published(&resolved);
        assert!(!actual.pending);
        assert_eq!(actual.support == Support::Unsupported, unsupported);
        assert_eq!(actual.uncertain, !unsupported);
        assert!(resolved.writes.is_empty());
        assert!(
            !resolved
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { .. }))
        );
    }
}

#[test]
fn disabled_goal_diagnostics_cannot_cross_newer_observations_or_a_rebound_actor() {
    for reconnect in [false, true] {
        let mut adapter = adapter();
        let issued = command(&mut adapter, Operation::Clear);
        if reconnect {
            let reattached = attach(&mut adapter);
            assert!(published(&reattached).pending);
            assert_eq!(published(&reattached).read, ReadState::Unknown);
        } else {
            let newer = adapter.receive(json!({"method":"thread/goal/updated", "params":{
                "threadId":"thread-exact", "goal":raw_goal("thread-exact", "New observation", "blocked")
            }})).unwrap();
            assert!(published(&newer).pending);
            assert!(!published(&newer).uncertain);
        }
        let current = snapshot(&mut adapter);
        assert!(current.pending);
        let released = adapter
            .receive(json!({"id":issued.writes[0]["id"], "error":{
                "code":-32600,"message":"goals feature is disabled"
            }}))
            .unwrap();
        let actual = published(&released);
        assert!(
            !actual.pending,
            "old actual completion releases only its physical slot"
        );
        assert_eq!(actual.read, current.read);
        assert_eq!(actual.support, current.support);
        assert_eq!(actual.uncertain, current.uncertain);
        if reconnect {
            assert!(actual.last_result.is_none());
        }
        assert!(released.writes.is_empty());
        assert!(
            !released
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { .. }))
        );
        // The suppression is observation-scoped, not a permanently muted
        // connection: a fresh exact Check refusal must still explain itself.
        let check = command(&mut adapter, Operation::Get);
        let current = adapter
            .receive(json!({"id":check.writes[0]["id"], "error":{
                "code":-32600,"message":"goals feature is disabled"
            }}))
            .unwrap();
        assert!(!published(&current).pending);
        assert!(published(&current).uncertain);
        assert_eq!(current.events.iter().filter(|event| matches!(event,
            Event::Error { fatal: false, message } if message == "Goals are disabled in this Codex connection. Enable Goals in the selected Codex configuration before starting a new connection. Your input is kept."
        )).count(), 1);
        assert!(current.writes.is_empty());
    }
}

#[test]
fn disabled_automatic_goal_probe_is_quiet_but_explicit_check_explains_and_chat_stays_usable() {
    let mut adapter = adapter();
    let attached = attach(&mut adapter);
    let reads = attached
        .writes
        .iter()
        .filter(|frame| frame["method"] == "thread/goal/get")
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1);
    let automatic = adapter
        .receive(json!({"id":reads[0]["id"], "error":{
            "code":-32600, "message":"goals feature is disabled"
        }}))
        .unwrap();
    assert!(!published(&automatic).pending);
    assert!(published(&automatic).uncertain);
    assert_eq!(published(&automatic).read, ReadState::Unknown);
    assert!(
        automatic.writes.is_empty(),
        "optional metadata failure does not retry or start work"
    );
    assert!(
        !automatic
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    assert!(adapter.ready && adapter.attached && adapter.direct_input);

    let explicit = command(&mut adapter, Operation::Get);
    assert_eq!(explicit.writes.len(), 1);
    assert_eq!(explicit.writes[0]["method"], "thread/goal/get");
    let checked = adapter
        .receive(json!({"id":explicit.writes[0]["id"], "error":{
            "code":-32600, "message":"goals feature is disabled"
        }}))
        .unwrap();
    assert!(!published(&checked).pending);
    assert!(published(&checked).uncertain);
    assert_eq!(published(&checked).read, ReadState::Unknown);
    assert_eq!(checked.events.iter().filter(|event| matches!(event,
        Event::Error { fatal: false, message } if message == "Goals are disabled in this Codex connection. Enable Goals in the selected Codex configuration before starting a new connection. Your input is kept."
    )).count(), 1);
    assert!(
        !checked
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: true, .. }))
    );
    assert!(checked.writes.is_empty());
    // This fixture has original-scope admission. The optional observation is
    // not a global ordinary-chat policy or a synthesized absent Goal.
    let ordinary = adapter
        .command(Command::Prompt {
            text: "Explicit ordinary chat remains ordinary".into(),
            attachments: vec![],
            mode: PromptMode::Send,
        })
        .unwrap();
    assert_eq!(ordinary.writes.len(), 1);
    assert_eq!(ordinary.writes[0]["method"], "turn/start");
    assert_eq!(ordinary.writes[0]["params"]["threadId"], "thread-exact");
}
