//! Original capture uses a fresh provider settings read immediately before
//! direct input dispatch. A bounded timeout preserves ordinary sending when
//! this optional metadata operation is unsupported or unresponsive.
use super::*;
use crate::chat::annotations::OriginalUpdate;
use crate::chat::protocol::original_proof::{self, ClaudeProofs};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub(super) struct Waiting {
    pub deadline: Instant,
    request: String,
    command: String,
    revision: u64,
    attention_generation: u64,
    provider_revision: u64,
    frames: Vec<Value>,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct Originals {
    pub staged: Option<String>,
    pub revision: u64,
    provider_revision: u64,
    #[cfg(test)]
    pub wait_timeout: Option<Duration>,
    pub updates: Vec<OriginalUpdate>,
    pub waiting: Option<Waiting>,
    proofs: ClaudeProofs,
    applied: BTreeMap<String, (chat::annotations::OriginalSettings, u64)>,
    policy: Option<(String, String)>,
    owned: BTreeSet<String>,
    started: BTreeSet<String>,
    init: Option<Value>,
    confirmed_session: Option<String>,
    latest_started: Option<String>,
}
impl Claude {
    pub(super) fn original_settings_changed(&mut self) {
        self.originals.revision = self.originals.revision.wrapping_add(1);
        // A setter ACK may only acknowledge the requested overlay. Until
        // the provider reports its effective connection policy again, never
        // combine an older init policy with a fresh model read.
        self.originals.policy = None;
        self.originals.init = None;
    }
    pub(super) fn original_frame_budget(&self, frame: &Value) -> Result<(), String> {
        if let Some(waiting) = &self.originals.waiting
            && encoded_len(frame)? > MAX_PAYLOAD.saturating_sub(waiting.bytes)
        {
            return Err("Claude pending settings and input exceed the native input budget".into());
        }
        Ok(())
    }
    pub(super) fn original_dispatch(
        &mut self,
        frame: Value,
        command: Option<String>,
    ) -> Result<Vec<Value>, String> {
        if let Some(waiting) = &mut self.originals.waiting {
            waiting.bytes = waiting.bytes.saturating_add(encoded_len(&frame)?);
            waiting.frames.push(frame);
            return Ok(vec![]);
        }
        let Some(command) = command else {
            return Ok(vec![frame]);
        };
        if self.originals.owned.len() >= 64
            && let Some(oldest) = self.originals.owned.iter().next().cloned()
        {
            // The durable intent retains unknown acceptance for lifecycle
            // cleanup. Retiring only the bounded resident join cannot grant
            // metadata authority or delete potentially consumed input.
            self.originals.proofs.cancel(&oldest);
            self.originals.applied.remove(&oldest);
            self.originals.started.remove(&oldest);
            self.originals.owned.remove(&oldest);
        }
        if self
            .originals
            .proofs
            .submit(
                &command,
                &command,
                self.originals.confirmed_session.as_deref(),
            )
            .is_err()
        {
            return Ok(vec![frame]);
        }
        self.originals.owned.insert(command.clone());
        // Read optional metadata first. Failure to allocate this optional RPC
        // cannot prevent dispatch of the user's already validated prompt.
        let read = match self.control(json!({"subtype":"get_settings"})) {
            Ok(read) => read,
            Err(_) => return Ok(vec![frame]),
        };
        let request = read.writes[0]["request_id"]
            .as_str()
            .ok_or("Claude settings request has no ID")?
            .to_owned();
        #[cfg(test)]
        let timeout = self
            .originals
            .wait_timeout
            .unwrap_or(Duration::from_secs(1));
        #[cfg(not(test))]
        let timeout = Duration::from_secs(1);
        self.originals.waiting = Some(Waiting {
            deadline: Instant::now() + timeout,
            request,
            command,
            revision: self.originals.revision,
            attention_generation: self.attention_generation,
            provider_revision: self.originals.provider_revision,
            bytes: encoded_len(&frame)?,
            frames: vec![frame],
        });
        Ok(read.writes)
    }
    pub(super) fn original_flush(&mut self) -> Vec<Value> {
        if let Some(waiting) = self.originals.waiting.take() {
            self.controls.remove(&waiting.request);
            waiting.frames
        } else {
            vec![]
        }
    }
    pub(super) fn original_abort(&mut self, closing: bool) -> Result<Option<Step>, String> {
        let Some(waiting) = self.originals.waiting.take() else {
            return Ok(None);
        };
        self.controls.remove(&waiting.request);
        self.originals.proofs.cancel(&waiting.command);
        self.originals.applied.remove(&waiting.command);
        self.originals.started.remove(&waiting.command);
        self.originals.owned.remove(&waiting.command);
        self.originals.updates.push(OriginalUpdate::Rejected {
            command_id: waiting.command.clone(),
        });
        let ids = if closing {
            waiting
                .frames
                .iter()
                .filter_map(|frame| frame["uuid"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        } else {
            vec![waiting.command.clone()]
        };
        let mut step = Step::default();
        self.retract(&json!(ids), &mut step)?;
        for id in &ids {
            self.submissions.remove(id);
            self.queued.retain(|queued| queued.id != *id);
        }
        step.events.push(Event::QueueChanged(self.queued.clone()));
        // Reset only the optimistic turn this held input introduced. An
        // actual newer provider turn remains busy and follows normal interrupt.
        if self.attention_generation == waiting.attention_generation
            && self.originals.provider_revision == waiting.provider_revision
        {
            self.status(false, "Pending input cancelled before dispatch", &mut step);
            step.events.push(Event::TurnSettled);
            step.events.push(Event::TurnFinished {
                key: None,
                outcome: crate::notifications::Kind::Cancelled,
            });
            self.attention_generation = self.attention_generation.wrapping_add(1);
            self.started_generation = None;
        }
        if !closing {
            step.writes.extend(waiting.frames.into_iter().skip(1));
        }
        Ok(Some(step))
    }
    pub(super) fn original_maintenance(&mut self) -> Step {
        if self
            .originals
            .waiting
            .as_ref()
            .is_some_and(|waiting| Instant::now() >= waiting.deadline)
        {
            let waiting = self.originals.waiting.take().unwrap();
            self.controls.remove(&waiting.request);
            return Step {
                writes: waiting.frames,
                ..Step::default()
            };
        }
        Step::default()
    }
    pub(super) fn original_control_response(&mut self, frame: &Value) -> Option<Step> {
        if frame["type"] != "control_response"
            || self.originals.waiting.as_ref().is_none_or(|waiting| {
                frame["response"]["request_id"].as_str() != Some(waiting.request.as_str())
            })
        {
            return None;
        }
        let waiting = self.originals.waiting.take().unwrap();
        self.controls.remove(&waiting.request);
        if frame["response"]["subtype"] == "success"
            && waiting.revision == self.originals.revision
            && self.originals.applied.len() < 64
            && let Ok(applied) =
                original_proof::claude_applied_model_effort(&frame["response"]["response"])
        {
            if let Some((permission, fast)) = &self.originals.policy
                && let Ok(settings) = original_proof::claude_applied_settings(
                    &json!({"applied":{"model":applied.model,"effort":applied.effort}}),
                    permission,
                    fast,
                )
            {
                self.originals
                    .applied
                    .insert(waiting.command, (settings, waiting.revision));
            }
        }
        Some(Step {
            writes: waiting.frames,
            ..Step::default()
        })
    }
    pub(super) fn original_connection_policy(&mut self, report: &Value) {
        if let (Some(permission), Some(fast)) = (
            report["current_permission_mode"].as_str(),
            report["fast_mode_state"].as_str(),
        ) {
            if permission.len() <= 64 && fast.len() <= 64 {
                self.originals.policy = Some((
                    (if permission == "default" {
                        "manual"
                    } else {
                        permission
                    })
                    .into(),
                    fast.into(),
                ));
            }
        }
    }
    pub(super) fn original_observe(&mut self, frame: &Value, step: &Step) {
        if frame["type"] == "conversation_reset" {
            if let Some(waiting) = self.originals.waiting.take() {
                self.controls.remove(&waiting.request);
                self.originals.updates.push(OriginalUpdate::Rejected {
                    command_id: waiting.command,
                });
            }
            self.originals.proofs = ClaudeProofs::default();
            self.originals.applied.clear();
            self.originals.init = None;
            self.originals.policy = None;
            self.originals.owned.clear();
            self.originals.started.clear();
            self.originals.confirmed_session = None;
            self.originals.latest_started = None;
            self.originals.revision = self.originals.revision.wrapping_add(1);
            return;
        }
        // The enclosing receive path already validated this actual provider
        // identity against the connection. Launch --session-id is not evidence.
        if let Some(session) = frame["session_id"].as_str() {
            self.originals.confirmed_session = Some(session.to_owned());
        }
        if frame["type"] == "system" && frame["subtype"] == "init" {
            let mut init = serde_json::Map::new();
            for key in [
                "type",
                "subtype",
                "session_id",
                "model",
                "effort",
                "permissionMode",
                "fast_mode_state",
            ] {
                if let Some(value) = frame.get(key) {
                    if value.as_str().is_some_and(|value| value.len() > 512) {
                        self.originals.revision = self.originals.revision.wrapping_add(1);
                        self.originals.policy = None;
                        return;
                    }
                    if value.is_string() || value.is_null() {
                        init.insert(key.into(), value.clone());
                    }
                }
            }
            let init = Value::Object(init);
            if self.originals.init.as_ref().is_some_and(|previous| {
                ["model", "effort", "permissionMode", "fast_mode_state"]
                    .iter()
                    .any(|key| previous.get(*key) != init.get(*key))
            }) {
                self.originals.revision = self.originals.revision.wrapping_add(1);
            }
            if self.originals.applied.values().any(|(settings, _)| {
                init["model"].as_str().is_some_and(|model| {
                    settings.model != chat::annotations::Setting::Value(model.into())
                }) || init["effort"].as_str().is_some_and(|effort| {
                    settings.reasoning != chat::annotations::Setting::Value(effort.into())
                })
            }) {
                self.originals.revision = self.originals.revision.wrapping_add(1);
            }
            self.originals.init = Some(init);
        } else if let Some(init) = self.originals.init.as_mut() {
            // Autonomous provider changes invalidate a previously sampled
            // setting. They must never silently become this prompt's settings.
            for key in ["permissionMode", "fast_mode_state"] {
                if let Some(value) = frame.get(key)
                    && init.get(key) != Some(value)
                {
                    self.originals.revision = self.originals.revision.wrapping_add(1);
                    if value.as_str().is_some_and(|value| value.len() <= 64) {
                        init[key] = value.clone();
                    } else {
                        init[key] = Value::Null;
                        self.originals.policy = None;
                    }
                }
            }
        }
        // New init metadata may validate or invalidate an existing policy
        // sample, but can never fill policy that was unknown at dispatch.
        if let Some(init) = &self.originals.init
            && let (Some(permission), Some(fast)) = (
                init["permissionMode"].as_str(),
                init["fast_mode_state"].as_str(),
            )
        {
            let permission = if permission == "default" {
                "manual"
            } else {
                permission
            };
            if self
                .originals
                .policy
                .as_ref()
                .is_some_and(|old| old.0 != permission || old.1 != fast)
            {
                self.originals.revision = self.originals.revision.wrapping_add(1);
            }
            self.originals.policy = Some((permission.into(), fast.into()));
        }
        if frame["type"] == "command_lifecycle"
            && let Some(command) = frame["command_uuid"].as_str()
        {
            match frame["state"].as_str() {
                Some("started" | "completed") => {
                    if frame["state"] == "started" {
                        self.originals.provider_revision =
                            self.originals.provider_revision.wrapping_add(1);
                    }
                    if self.originals.owned.contains(command) {
                        self.originals.started.insert(command.to_owned());
                    }
                    if frame["state"] == "started" {
                        self.originals.latest_started = Some(command.to_owned());
                    }
                }
                Some("refused" | "cancelled" | "discarded") => {
                    // A terminal before consumption proves non-execution.
                    // After started/completed, cancellation can mean a failed
                    // turn whose delayed user echo must retain its original.
                    if self.originals.owned.contains(command)
                        && !self.originals.started.contains(command)
                    {
                        self.originals.applied.remove(command);
                        self.originals.owned.remove(command);
                        self.originals.updates.push(OriginalUpdate::Rejected {
                            command_id: command.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some(session) = self.originals.confirmed_session.as_ref()
            && let Some(user) = self.originals.latest_started.take()
        {
            self.originals.updates.push(OriginalUpdate::Latest {
                session_id: session.clone(),
                user_id: user,
            });
        }
        let aliases = if frame["type"] == "user" {
            step.events
                .iter()
                .filter_map(|event| match event {
                    Event::Item(item) if item.role == Role::User => Some(item.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        } else {
            vec![]
        };
        if let Some(proof) = self.originals.proofs.observe(
            frame,
            &aliases,
            self.originals.confirmed_session.as_deref(),
        ) {
            self.originals.owned.remove(&proof.command_id);
            self.originals.started.remove(&proof.command_id);
            // Settings are immutable at dispatch. Once this exact join is
            // complete, absent/stale samples can never become eligible later.
            if let Some((settings, revision)) = self.originals.applied.remove(&proof.command_id)
                && revision == self.originals.revision
            {
                self.originals.updates.push(OriginalUpdate::Settings {
                    command_id: proof.command_id.clone(),
                    settings,
                });
                self.originals.updates.push(OriginalUpdate::Accepted {
                    command_id: proof.command_id,
                    session_id: proof.session_id,
                    user_id: proof.user_id,
                    turn_id: proof.turn_id,
                    aliases: proof.aliases,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adapter() -> Claude {
        let mut adapter = Claude {
            ready: true,
            async_input: true,
            ..Claude::default()
        };
        adapter.original_connection_policy(
            &json!({"current_permission_mode":"default","fast_mode_state":"off"}),
        );
        adapter
    }
    fn prompt(text: &str, mode: PromptMode) -> Command {
        Command::Prompt {
            text: text.into(),
            attachments: vec![],
            mode,
        }
    }
    fn settings_reply(request: &Value) -> Value {
        json!({"type":"control_response","response":{"request_id":request["request_id"],"subtype":"success","response":{"applied":{"model":"reported-model","effort":"low"}}}})
    }
    fn init() -> Value {
        json!({"type":"system","subtype":"init","session_id":"00000000-0000-4000-8000-000000000001","model":"reported-model","effort":"low","permissionMode":"default","fast_mode_state":"off"})
    }
    fn user(command: &str) -> Value {
        json!({"type":"user","uuid":command,"session_id":"00000000-0000-4000-8000-000000000001","parent_tool_use_id":null,"message":{"id":"actual-provider-root","role":"user","content":[{"type":"text","text":"provider-expanded echo"}]}})
    }
    fn started(command: &str) -> Value {
        json!({"type":"command_lifecycle","command_uuid":command,"state":"started"})
    }
    #[test]
    fn actual_decoder_first_prompt_joins_dispatch_settings_native_session_user_and_lifecycle() {
        for echo_first in [false, true] {
            let mut adapter = adapter();
            adapter.original_command(Some("exact-command".into()));
            let read = adapter
                .command(prompt("original /command", PromptMode::Send))
                .unwrap();
            assert_eq!(read.writes[0]["request"]["subtype"], "get_settings");
            assert!(read.writes.iter().all(|frame| frame["type"] != "user"));
            let dispatched = adapter.receive(settings_reply(&read.writes[0])).unwrap();
            assert_eq!(dispatched.writes[0]["uuid"], "exact-command");
            adapter.receive(init()).unwrap();
            assert!(adapter.take_original_updates().is_empty());
            if echo_first {
                adapter.receive(user("exact-command")).unwrap();
                adapter.receive(started("exact-command")).unwrap();
            } else {
                adapter.receive(started("exact-command")).unwrap();
                adapter.receive(user("exact-command")).unwrap();
            }
            let updates = adapter.take_original_updates();
            assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Settings{command_id,settings} if command_id=="exact-command"&&settings.model==chat::annotations::Setting::Value("reported-model".into()))));
            assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Accepted{command_id,user_id,aliases,..} if command_id=="exact-command"&&user_id=="exact-command"&&!aliases.is_empty())));
            assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Latest{user_id,..} if user_id=="exact-command")));
        }
    }
    #[test]
    fn mutating_command_flushes_original_input_before_setting_change_and_invalidates_proof() {
        let mut adapter = adapter();
        adapter.original_command(Some("command".into()));
        let read = adapter
            .command(prompt("original", PromptMode::Send))
            .unwrap();
        let change = adapter
            .command(Command::SetModel {
                model: "new-model".into(),
                reasoning: Some("high".into()),
            })
            .unwrap();
        assert_eq!(change.writes[0]["type"], "user");
        assert_eq!(change.writes[1]["request"]["subtype"], "set_model");
        assert!(
            adapter
                .receive(settings_reply(&read.writes[0]))
                .unwrap()
                .writes
                .is_empty()
        );
        adapter.receive(init()).unwrap();
        adapter.receive(started("command")).unwrap();
        adapter.receive(user("command")).unwrap();
        assert!(
            adapter
                .take_original_updates()
                .iter()
                .all(|update| !matches!(
                    update,
                    OriginalUpdate::Settings { .. } | OriginalUpdate::Accepted { .. }
                ))
        );
    }
    #[test]
    fn setting_ack_never_reuses_prior_init_policy_for_the_next_original() {
        for change in [
            Command::SetPermissionMode { mode: "bypassPermissions".into() },
            Command::SetFastMode { enabled: true },
        ] {
            let mut adapter = adapter();
            adapter.fast_mode = Some(chat::FastMode { state: chat::FastModeState::Off, disabled_reason: None });
            adapter.receive(init()).unwrap();
            let control = adapter.command(change).unwrap();
            adapter.receive(json!({"type":"control_response","response":{"request_id":control.writes[0]["request_id"],"subtype":"success","response":{"mode":"bypassPermissions"}}})).unwrap();
            assert!(adapter.originals.policy.is_none());
            assert!(adapter.originals.init.is_none());
            adapter.original_command(Some("after-setting".into()));
            let read = adapter.command(prompt("original", PromptMode::Send)).unwrap();
            adapter.receive(settings_reply(&read.writes[0])).unwrap();
            let mut changed = init();
            changed["permissionMode"] = json!("bypassPermissions");
            changed["fast_mode_state"] = json!("on");
            adapter.receive(changed).unwrap();
            adapter.receive(started("after-setting")).unwrap();
            adapter.receive(user("after-setting")).unwrap();
            assert!(adapter.take_original_updates().iter().all(|update| !matches!(update, OriginalUpdate::Settings { .. } | OriginalUpdate::Accepted { .. })));
        }
    }

    #[test]
    fn timeout_and_queued_input_preserve_wire_order_without_inventing_settings() {
        let mut adapter = adapter();
        adapter.original_command(Some("command".into()));
        let _ = adapter.command(prompt("first", PromptMode::Send)).unwrap();
        let queued = adapter
            .command(prompt("second", PromptMode::Queue))
            .unwrap();
        assert!(queued.writes.is_empty());
        adapter.originals.waiting.as_mut().unwrap().deadline = Instant::now();
        let flushed = adapter.maintenance().unwrap();
        assert_eq!(flushed.writes.len(), 2);
        assert_eq!(flushed.writes[0]["uuid"], "command");
        assert_eq!(flushed.writes[0]["message"]["content"][0]["text"], "first");
        assert_eq!(flushed.writes[1]["message"]["content"][0]["text"], "second");
        assert!(adapter.maintenance_deadline().is_none());
        assert!(adapter.maintenance().unwrap().writes.is_empty());
        adapter.receive(init()).unwrap();
        adapter.receive(started("command")).unwrap();
        adapter.receive(user("command")).unwrap();
        assert!(
            adapter
                .take_original_updates()
                .iter()
                .all(|update| !matches!(
                    update,
                    OriginalUpdate::Settings { .. } | OriginalUpdate::Accepted { .. }
                ))
        );
    }
    #[test]
    fn repeated_init_or_first_init_with_changed_policy_never_mixes_dispatch_settings() {
        for initial_init in [false, true] {
            let mut adapter = adapter();
            if initial_init {
                adapter.receive(init()).unwrap();
            }
            adapter.original_command(Some("command".into()));
            let read = adapter
                .command(prompt("original", PromptMode::Send))
                .unwrap();
            adapter.receive(settings_reply(&read.writes[0])).unwrap();
            let mut changed = init();
            changed["permissionMode"] = json!("bypassPermissions");
            changed["fast_mode_state"] = json!("on");
            adapter.receive(changed).unwrap();
            adapter.receive(started("command")).unwrap();
            adapter.receive(user("command")).unwrap();
            assert!(
                adapter
                    .take_original_updates()
                    .iter()
                    .all(|update| !matches!(
                        update,
                        OriginalUpdate::Settings { .. } | OriginalUpdate::Accepted { .. }
                    ))
            );
        }
    }
    #[test]
    fn cancelled_started_turn_preserves_delayed_user_join_but_prestart_rejection_retires() {
        for started_first in [false, true] {
            let mut adapter = adapter();
            adapter.original_command(Some("command".into()));
            let read = adapter
                .command(prompt("original", PromptMode::Send))
                .unwrap();
            adapter.receive(settings_reply(&read.writes[0])).unwrap();
            adapter.receive(init()).unwrap();
            if started_first {
                adapter.receive(started("command")).unwrap();
            }
            adapter.receive(json!({"type":"command_lifecycle","command_uuid":"command","state":"cancelled","reason":"model_error"})).unwrap();
            adapter.receive(user("command")).unwrap();
            let updates = adapter.take_original_updates();
            assert_eq!(updates.iter().any(|update|matches!(update,OriginalUpdate::Rejected{command_id} if command_id=="command")),!started_first);
            assert_eq!(updates.iter().any(|update|matches!(update,OriginalUpdate::Accepted{command_id,..} if command_id=="command")),started_first);
        }
    }
    #[test]
    fn missing_policy_before_dispatch_cannot_be_filled_from_a_later_init() {
        let mut adapter = Claude {
            ready: true,
            ..Claude::default()
        };
        adapter.original_command(Some("command".into()));
        let read = adapter
            .command(prompt("original", PromptMode::Send))
            .unwrap();
        adapter.receive(settings_reply(&read.writes[0])).unwrap();
        adapter.receive(init()).unwrap();
        adapter.receive(started("command")).unwrap();
        adapter.receive(user("command")).unwrap();
        assert!(
            adapter
                .take_original_updates()
                .iter()
                .all(|update| !matches!(
                    update,
                    OriginalUpdate::Settings { .. } | OriginalUpdate::Accepted { .. }
                ))
        );
    }

    #[test]
    fn more_than_64_unknown_turns_do_not_disable_later_qualified_capture() {
        let mut adapter = adapter();
        adapter.receive(init()).unwrap();
        for n in 0..80 {
            let id = format!("unknown-{n}");
            adapter.original_command(Some(id.clone()));
            adapter
                .command(prompt("original", PromptMode::Send))
                .unwrap();
            adapter.originals.waiting.as_mut().unwrap().deadline = Instant::now();
            assert_eq!(adapter.maintenance().unwrap().writes.len(), 1);
            adapter.receive(started(&id)).unwrap();
            adapter.receive(user(&id)).unwrap();
            adapter.receive(json!({"type":"result","subtype":"success","session_id":"00000000-0000-4000-8000-000000000001","is_error":false,"result":"done"})).unwrap();
            adapter.take_original_updates();
        }
        adapter.original_command(Some("qualified".into()));
        let read = adapter
            .command(prompt("eligible", PromptMode::Send))
            .unwrap();
        adapter.receive(settings_reply(&read.writes[0])).unwrap();
        adapter.receive(started("qualified")).unwrap();
        adapter.receive(user("qualified")).unwrap();
        assert!(adapter.take_original_updates().iter().any(|update|matches!(update,OriginalUpdate::Accepted{command_id,..} if command_id=="qualified")));
    }

    #[test]
    fn interruption_and_close_never_dispatch_held_direct_input_or_late_getter_receipts() {
        for closing in [false, true] {
            let mut adapter = adapter();
            adapter.original_command(Some("never-dispatched".into()));
            let read = adapter.command(prompt("direct", PromptMode::Send)).unwrap();
            adapter
                .command(prompt("separate queued input", PromptMode::Queue))
                .unwrap();
            let aborted = if closing {
                adapter.discard_undispatched().unwrap().unwrap()
            } else {
                adapter.command(Command::Interrupt).unwrap()
            };
            assert!(!adapter.busy);
            assert!(
                aborted
                    .writes
                    .iter()
                    .all(|frame| frame["uuid"] != "never-dispatched")
            );
            assert_eq!(aborted.writes.len(), usize::from(!closing));
            if !closing {
                assert_eq!(
                    aborted.writes[0]["message"]["content"][0]["text"],
                    "separate queued input"
                );
            }
            assert!(
                adapter
                    .receive(settings_reply(&read.writes[0]))
                    .unwrap()
                    .writes
                    .is_empty()
            );
            assert!(adapter.maintenance().unwrap().writes.is_empty());
            let updates = adapter.take_original_updates();
            assert!(updates.iter().any(|update|matches!(update,OriginalUpdate::Rejected{command_id} if command_id=="never-dispatched")));
            assert!(updates.iter().all(|update| !matches!(
                update,
                OriginalUpdate::Latest { .. } | OriginalUpdate::Accepted { .. }
            )));
        }
    }

    #[test]
    fn aborting_unsent_input_does_not_settle_a_newer_actual_provider_turn() {
        let mut adapter = adapter();
        adapter.original_command(Some("held".into()));
        adapter.command(prompt("direct", PromptMode::Send)).unwrap();
        adapter.receive(json!({"type":"command_lifecycle","command_uuid":"newer-native-turn","session_id":"00000000-0000-4000-8000-000000000001","state":"started"})).unwrap();
        let generation = adapter.attention_generation;
        let interrupted = adapter.command(Command::Interrupt).unwrap();
        assert!(adapter.busy);
        assert_eq!(adapter.attention_generation, generation);
        assert!(
            interrupted
                .events
                .iter()
                .all(|event| !matches!(event, Event::TurnSettled | Event::TurnFinished { .. }))
        );
        assert!(
            interrupted
                .writes
                .iter()
                .any(|frame| frame["request"]["subtype"] == "interrupt")
        );
        assert!(
            interrupted
                .writes
                .iter()
                .all(|frame| frame["type"] != "user")
        );
    }
}
