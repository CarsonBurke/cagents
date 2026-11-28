use super::*;
use crate::chat::{ChangeKind, CommandAction, ToolStatus};

/// Antigravity 1.5.12's `initialize` reply, as captured.
fn agy_initialize() -> Value {
    json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true, "load_session": true,
        "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
        "sessionCapabilities": {"resume": {}, "close": {}, "load": {}, "list": {}, "additionalDirectories": {}},
        "auth": {"logout": {}}, "_meta": {"antigravity": {"tier": true, "systemPrompt": {"append": true}}}},
        "agentInfo": {"name": "Antigravity ACP Server", "version": "1.5.12"},
        "authMethods": [{"id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account"}]})
}

/// Antigravity 1.5.12's `session/new` reply, as captured, with its model
/// lists shortened and its account tier replaced.
fn agy_new() -> Value {
    json!({"sessionId": "98608bab-5eb6-4cf5-8f9b-46019bf29df7",
        // agy lists a combined identifier for each level a model offers:
        // Gemini 3.8 Flash has no extra-high level.
        "models": {"availableModels": [
            {"modelId": "gemini-4-neon-high", "name": "Gemini 4 Neon (High)"},
            {"modelId": "gemini-4-neon-xhigh", "name": "Gemini 4 Neon (XHigh)"},
            {"modelId": "gemini-4-neon-medium", "name": "Gemini 4 Neon (Medium)"},
            {"modelId": "gemini-4-neon-low", "name": "Gemini 4 Neon (Low)"},
            {"modelId": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)"},
            {"modelId": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)"},
            {"modelId": "gemini-3.8-flash-medium", "name": "Gemini 3.8 Flash (Medium)"}],
            "currentModelId": "gemini-4-neon-high"},
        "modes": {"currentModeId": "default", "availableModes": [
            {"id": "default", "name": "Default", "description": "Ask before edits and commands, per your permission settings."},
            {"id": "accept-edits", "name": "Accept edits", "description": "Approve file edits in the workspace without asking."},
            {"id": "auto", "name": "Auto", "description": "Approve edits and safe commands; ask before irreversible actions."},
            {"id": "plan", "name": "Plan", "description": "Plan carefully before executing a task."}]},
        "configOptions": [
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "gemini-4-neon",
             "options": [{"value": "gemini-4-neon", "name": "Gemini 4 Neon"}, {"value": "gemini-3.8-flash", "name": "Gemini 3.8 Flash"}]},
            {"id": "thought_level", "name": "Thinking Level", "category": "thought_level", "type": "select", "currentValue": "high",
             "options": [{"value": "low", "name": "Low"}, {"value": "medium", "name": "Medium"}, {"value": "high", "name": "High"}, {"value": "xhigh", "name": "Extra"}]},
            {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "default",
             "options": [{"value": "default", "name": "Default"}, {"value": "accept-edits", "name": "Accept edits"}, {"value": "auto", "name": "Auto"}, {"value": "plan", "name": "Plan"}]}],
        "_meta": {"antigravity": {"tier": {"name": "Fixture tier", "id": "fixture"}}}})
}

/// Muse 1.4.23's `initialize` and `session/new` replies, as captured.
fn muse_initialize() -> Value {
    json!({"agentCapabilities": {"loadSession": true, "mcpCapabilities": {"http": true, "sse": false},
        "promptCapabilities": {"audio": false, "embeddedContext": true, "image": true},
        "sessionCapabilities": {"additionalDirectories": {}, "list": {}, "resume": {}}},
        "agentInfo": {"name": "muse", "version": "1.4.23"},
        "authMethods": [{"description": "Sign in with a device code shown in your browser.", "id": "muse-login", "name": "Log in to Muse"}],
        "protocolVersion": 1})
}

fn muse_new() -> Value {
    json!({"configOptions": [
        {"category": "mode", "currentValue": "onRequest", "id": "mode", "name": "Mode", "options": [
            {"description": "Run every action without asking for approval", "name": "Allow all", "value": "allowAll"},
            {"description": "Ask for approval only when the action requires it", "name": "Ask on request", "value": "onRequest"}],
         "type": "select"},
        {"category": "model", "currentValue": "muse-wonder-full-1.0", "id": "model", "name": "Model", "options": [
            {"name": "muse-spark-1.2", "value": "muse-spark-1.2"}, {"name": "muse-wonder-full-1.0", "value": "muse-wonder-full-1.0"}],
         "type": "select"}],
        "modes": {"availableModes": [
            {"description": "Run every action without asking for approval", "id": "allowAll", "name": "Allow all"},
            {"description": "Ask for approval only when the action requires it", "id": "onRequest", "name": "Ask on request"}],
            "currentModeId": "onRequest"},
        "sessionId": "01a1f155-1f9e-7081-b35f-88ead0a9187d"})
}

fn launch(arguments: &[&str], session: Option<&str>) -> Launch {
    Launch {
        provider: crate::agent::Provider::Omp,
        recipe: crate::model::LaunchRecipe {
            program: "/opt/fake-acp-agent".into(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            working_directory: "/work/project".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        session_id: session.map(str::to_owned),
        draft_model: None,
    }
}

fn adapter(arguments: &[&str], session: Option<&str>) -> Acp {
    prepare(&TEST_PROFILE, &launch(arguments, session))
        .unwrap()
        .1
}

fn reply(request: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
}

fn refusal(request: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}})
}

fn update(session: &str, update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}})
}

fn only_write(step: &Step) -> &Value {
    assert_eq!(step.writes.len(), 1, "{:?}", step.writes);
    &step.writes[0]
}

/// Drives a fresh agy-shaped conversation to ready and returns its session.
fn ready(acp: &mut Acp) -> String {
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let mut step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
    while let Some(setting) = step.writes.first().cloned() {
        let options = agy_new()["configOptions"].clone();
        step = acp
            .receive(reply(&setting, json!({"configOptions": options})))
            .unwrap();
    }
    assert!(acp.stage == Stage::Ready);
    acp.session.clone().unwrap()
}

fn prompt(acp: &mut Acp, text: &str) -> Value {
    let step = acp
        .command(Command::Prompt {
            text: text.into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        })
        .unwrap();
    only_write(&step).clone()
}

fn items(step: &Step) -> Vec<&Item> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::Item(item) => Some(item),
            _ => None,
        })
        .collect()
}

fn finished(step: &Step) -> Option<Kind> {
    step.events.iter().find_map(|event| match event {
        Event::TurnFinished { outcome, .. } => Some(*outcome),
        _ => None,
    })
}

#[test]
fn prompt_activity_becomes_a_confirmed_turn_only_after_provider_evidence() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let start = |acp: &mut Acp| {
        let step = acp
            .command(Command::Prompt {
                text: "Start work".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            })
            .unwrap();
        assert!(step.events.contains(&Event::TurnStarted));
        assert!(!step.events.contains(&Event::TurnStartConfirmed));
        only_write(&step).clone()
    };
    let prompt = start(&mut acp);
    let refused = acp
        .receive(refusal(&prompt, -32000, "No allowance"))
        .unwrap();
    assert!(!refused.events.contains(&Event::TurnStartConfirmed));
    assert_eq!(finished(&refused), Some(Kind::Failed));

    let tool = json!({"sessionUpdate":"tool_call","toolCallId":"parent-work","status":"in_progress","kind":"execute","title":"Run checks"});
    let prompt = start(&mut acp);
    let first_tool = acp.receive(update(&session, tool.clone())).unwrap();
    assert!(first_tool.events.contains(&Event::TurnStartConfirmed));
    let completed = acp
        .receive(reply(&prompt, json!({"stopReason":"end_turn"})))
        .unwrap();
    assert!(!completed.events.contains(&Event::TurnStartConfirmed));

    let prompt = start(&mut acp);
    let late_tool = acp.receive(update(&session, tool)).unwrap();
    assert!(!late_tool.events.contains(&Event::TurnStartConfirmed));
    let response = json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Work began"}});
    let other = acp
        .receive(update("different-session", response.clone()))
        .unwrap();
    assert!(!other.events.contains(&Event::TurnStartConfirmed));
    let first = acp.receive(update(&session, response.clone())).unwrap();
    assert!(first.events.contains(&Event::TurnStartConfirmed));
    let next = acp.receive(update(&session, response)).unwrap();
    assert!(!next.events.contains(&Event::TurnStartConfirmed));
    let completed = acp
        .receive(reply(&prompt, json!({"stopReason":"end_turn"})))
        .unwrap();
    assert!(!completed.events.contains(&Event::TurnStartConfirmed));

    let prompt = start(&mut acp);
    let completed = acp
        .receive(reply(&prompt, json!({"stopReason":"end_turn"})))
        .unwrap();
    assert_eq!(
        completed.events.iter().find(|event| matches!(
            event,
            Event::TurnStartConfirmed | Event::TurnFinished { .. }
        )),
        Some(&Event::TurnStartConfirmed)
    );
    assert_eq!(finished(&completed), Some(Kind::Succeeded));
}

#[test]
fn the_handshake_offers_no_files_or_terminal_and_names_cagents() {
    let mut acp = adapter(&[], None);
    let step = acp.initialize().unwrap();
    let initialize = only_write(&step);
    assert_eq!(initialize["method"], "initialize");
    assert_eq!(initialize["params"]["protocolVersion"], 1);
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
    );
    assert_eq!(initialize["params"]["clientInfo"]["name"], "cagents");
    assert!(acp.initialization_pending());
    let opened = acp.receive(reply(initialize, agy_initialize())).unwrap();
    let new = only_write(&opened);
    assert_eq!(new["method"], "session/new");
    assert_eq!(
        new["params"],
        json!({"cwd": "/work/project", "mcpServers": []})
    );
}

#[test]
fn agy_settings_apply_in_order_before_the_conversation_is_ready() {
    let mut acp = adapter(
        &[
            "--model",
            "gemini-3.8-flash",
            "--reasoning",
            "low",
            "--mode",
            "plan",
        ],
        None,
    );
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Session { id, .. } if id == "98608bab-5eb6-4cf5-8f9b-46019bf29df7")));
    // Config options win over the unstable `models` list: two base models,
    // each with its own thinking levels, not seven combined identifiers.
    let models = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Models {
                models,
                selected,
                reasoning,
                ..
            } => Some((models.clone(), selected.clone(), reasoning.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(models.0.len(), 2);
    assert_eq!(models.0[1].id, "gemini-3.8-flash");
    assert_eq!(models.0[0].reasoning, ["low", "medium", "high", "xhigh"]);
    assert_eq!(models.0[1].reasoning, ["low", "medium", "high"]);
    assert_eq!(models.1.as_deref(), Some("gemini-4-neon"));
    assert_eq!(models.2.as_deref(), Some("high"));
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Ready(true)))
    );
    let mut sent = Vec::new();
    let mut step = step;
    let mut options = agy_new()["configOptions"].clone();
    while let Some(setting) = step.writes.first().cloned() {
        assert_eq!(step.writes.len(), 1, "one setting at a time");
        assert!(acp.initialization_pending());
        assert_eq!(setting["method"], "session/set_config_option");
        let (id, value) = (
            setting["params"]["configId"].as_str().unwrap().to_owned(),
            setting["params"]["value"].clone(),
        );
        for option in options.as_array_mut().unwrap() {
            if option["id"] == id.as_str() {
                option["currentValue"] = value.clone();
            }
        }
        sent.push((id, value));
        step = acp
            .receive(reply(&setting, json!({"configOptions": options})))
            .unwrap();
    }
    assert_eq!(
        sent,
        [
            ("model".to_owned(), json!("gemini-3.8-flash")),
            ("thought_level".to_owned(), json!("low")),
            ("mode".to_owned(), json!("plan")),
        ]
    );
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::Ready(true)))
    );
    assert!(step.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "gemini-3.8-flash" && model.reasoning.as_deref() == Some("low"))));
    assert!(step.events.iter().any(|event| matches!(event,
        Event::PermissionMode(Some(mode)) if mode == "plan")));
    let capabilities = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Capabilities(capabilities) => Some(capabilities.clone()),
            _ => None,
        })
        .unwrap();
    assert!(capabilities.model_selection && capabilities.reasoning_selection);
    assert!(capabilities.steer && capabilities.queue && !capabilities.compact);
    // By meaning: the test profile's `acceptEdits` is agy's `accept-edits`.
    assert_eq!(
        capabilities.permission_modes,
        ["default", "acceptEdits", "auto", "plan"]
    );
    assert!(!acp.initialization_pending());
}

#[test]
fn startup_requires_a_fresh_mode_confirmation_from_the_requested_option() {
    for response in [
        json!({}),
        json!({"configOptions": []}),
        json!({"configOptions": [{
            "id": "other", "category": "mode", "type": "select",
            "currentValue": "default", "options": [{"value": "default"}]
        }]}),
    ] {
        let mut acp = adapter(&["--mode", "default"], None);
        let initialize = acp.initialize().unwrap();
        let opened = acp
            .receive(reply(only_write(&initialize), agy_initialize()))
            .unwrap();
        let step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
        let step = acp.receive(reply(only_write(&step), response)).unwrap();
        assert!(matches!(acp.stage, Stage::Failed));
        assert!(step.writes.is_empty());
        assert!(step.events.iter().any(|event| matches!(event,
            Event::Error { message, fatal: true } if message.contains("did not confirm permission mode"))));
        assert!(
            !step
                .events
                .iter()
                .any(|event| matches!(event, Event::Ready(true)))
        );
    }
}

#[test]
fn a_mode_update_before_dispatch_cannot_change_the_first_correlated_prompt() {
    for changed in [
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "auto"}),
        // The older mode state still lists the startup default. Once the
        // preferred option is gone, that stale value cannot confirm it.
        json!({"sessionUpdate": "config_option_update", "configOptions": []}),
    ] {
        let mut acp = adapter(&["--mode", "default"], None);
        let session = ready(&mut acp);
        acp.receive(update(&session, changed)).unwrap();
        let command = Command::Prompt {
            text: "Read the fixture".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        };
        assert!(
            acp.command_for_run(crate::model::RunId::new(), command)
                .unwrap_err()
                .contains("has not confirmed permission mode")
        );
    }
}

#[test]
fn startup_can_use_the_legacy_mode_acknowledgment() {
    let mut acp = adapter(&["--mode", "plan"], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let mut new = agy_new();
    new.as_object_mut().unwrap().remove("configOptions");
    let step = acp.receive(reply(only_write(&opened), new)).unwrap();
    let setting = only_write(&step);
    assert_eq!(setting["method"], "session/set_mode");
    assert_eq!(setting["params"]["modeId"], "plan");
    let step = acp.receive(reply(setting, json!({}))).unwrap();
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::Ready(true)))
    );
    assert_eq!(acp.config.current_mode(), Some("plan"));
}

/// agy's thinking option lists Extra for every model, but Flash's combined
/// identifiers say it has none: a launch asking for it is not sent it.
#[test]
fn agy_is_never_sent_a_level_the_chosen_model_lacks() {
    let mut acp = adapter(
        &["--model", "gemini-3.8-flash", "--reasoning", "xhigh"],
        None,
    );
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
    let setting = only_write(&step).clone();
    assert_eq!(setting["params"]["configId"], "model");
    let mut options = agy_new()["configOptions"].clone();
    options[0]["currentValue"] = json!("gemini-3.8-flash");
    options[1]["currentValue"] = json!("xhigh");
    let step = acp
        .receive(reply(&setting, json!({"configOptions": options})))
        .unwrap();
    assert!(step.writes.is_empty(), "{:?}", step.writes);
    // Nor is Flash reported running at a level it doesn't have.
    assert!(step.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "gemini-3.8-flash" && model.reasoning.is_none())));
}

#[test]
fn muse_offers_models_and_modes_without_reasoning_levels() {
    let mut acp = adapter(&["--model", "muse-spark-1.2", "--reasoning", "high"], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let step = acp.receive(reply(only_write(&opened), muse_new())).unwrap();
    let setting = only_write(&step).clone();
    assert_eq!(setting["params"]["configId"], "model");
    let mut options = muse_new()["configOptions"].clone();
    options[1]["currentValue"] = json!("muse-spark-1.2");
    let step = acp
        .receive(reply(&setting, json!({"configOptions": options})))
        .unwrap();
    // A reasoning level chosen with a model that offers none is left
    // behind with the model it was chosen for, not reported or sent.
    assert!(step.writes.is_empty());
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    let capabilities = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Capabilities(capabilities) => Some(capabilities.clone()),
            _ => None,
        })
        .unwrap();
    assert!(capabilities.model_selection && !capabilities.reasoning_selection);
    assert_eq!(capabilities.permission_modes, ["allowAll", "onRequest"]);
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::Ready(true)))
    );
}

/// The reasoning levels each `Event::Models` entry offers, by model.
fn levels(step: &Step) -> Vec<(String, Vec<String>)> {
    step.events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::Models { models, .. } => Some(
                models
                    .iter()
                    .map(|model| (model.id.clone(), model.reasoning.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap()
}

#[test]
fn a_muse_model_offers_only_the_levels_it_listed_while_current() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let mut options = muse_new()["configOptions"].clone();
    options[1]["currentValue"] = json!("muse-spark-1.2");
    options.as_array_mut().unwrap().push(json!(
        {"category": "thought_level", "currentValue": "xhigh", "id": "thought_level",
         "name": "Reasoning effort", "type": "select",
         "options": [{"name": "Use this for deepest analysis and complex fixes.", "value": "xhigh"}]}));
    let mut new = muse_new();
    new["configOptions"] = options.clone();
    let step = acp.receive(reply(only_write(&opened), new)).unwrap();
    // Spark's level is not wonder's: the option lists only the current
    // model's levels, and wonder has not said what it offers.
    let expect = |spark: &[&str]| {
        vec![
            (
                "muse-spark-1.2".to_owned(),
                spark.iter().map(|level| (*level).to_owned()).collect(),
            ),
            ("muse-wonder-full-1.0".to_owned(), Vec::new()),
        ]
    };
    assert_eq!(levels(&step), expect(&["xhigh"]));
    let mut snapshot = crate::chat::Snapshot::default();
    crate::chat::runtime::project_events_for_test(&mut snapshot, step.events);
    let initial = snapshot.reasoning_observed_at.clone().unwrap();
    assert_eq!(initial.keys().collect::<Vec<_>>(), ["muse-spark-1.2"]);

    // Wonder, once current, lists no option; spark keeps the level it had.
    options.as_array_mut().unwrap().pop();
    options[1]["currentValue"] = json!("muse-wonder-full-1.0");
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "01a1f155-1f9e-7081-b35f-88ead0a9187d",
            "update": {"sessionUpdate": "config_option_update", "configOptions": options}}}),
        )
        .unwrap();
    assert_eq!(levels(&step), expect(&["xhigh"]));
    crate::chat::runtime::project_events_for_test(&mut snapshot, step.events);
    let observed = snapshot.reasoning_observed_at.unwrap();
    assert_eq!(observed["muse-spark-1.2"], initial["muse-spark-1.2"]);
    assert!(observed.contains_key("muse-wonder-full-1.0"));
    assert!(snapshot.models[1].reasoning.is_empty());
}

#[test]
fn a_reasoning_level_is_reported_unless_a_new_model_explains_its_absence() {
    let ready = |arguments: &[&str]| {
        let mut acp = adapter(arguments, None);
        let initialize = acp.initialize().unwrap();
        let opened = acp
            .receive(reply(only_write(&initialize), muse_initialize()))
            .unwrap();
        let step = acp.receive(reply(only_write(&opened), muse_new())).unwrap();
        (acp, step)
    };
    // A level asked for alone, which the agent has no option for, is
    // reported rather than sent.
    let (_, step) = ready(&["--reasoning", "high"]);
    assert!(step.writes.is_empty());
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Error { message, fatal: false } if message.contains("reasoning level"))));

    // A switch to a model without the conversation's level, as from Muse's
    // spark-1.2 to wonder, changes the model and nothing else.
    let (mut acp, _) = ready(&[]);
    let step = acp
        .command(Command::SetModel {
            model: "muse-spark-1.2".into(),
            reasoning: Some("xhigh".into()),
        })
        .unwrap();
    let setting = only_write(&step).clone();
    let mut options = muse_new()["configOptions"].clone();
    options[1]["currentValue"] = json!("muse-spark-1.2");
    options
        .as_array_mut()
        .unwrap()
        .push(json!({"category": "thought_level",
        "currentValue": "low", "id": "thought_level", "name": "Reasoning effort",
        "options": [{"name": "Low", "value": "low"}, {"name": "Extra high", "value": "xhigh"}],
        "type": "select"}));
    let step = acp
        .receive(reply(&setting, json!({"configOptions": options})))
        .unwrap();
    // spark-1.2 offers the level, so it is still asked for.
    assert_eq!(only_write(&step)["params"]["configId"], "thought_level");
    assert_eq!(only_write(&step)["params"]["value"], "xhigh");
    assert!(acp.first_model_pending());
    assert!(acp.accepts_turn().is_err());
    options[2]["currentValue"] = json!("xhigh");
    let step = acp
        .receive(reply(only_write(&step), json!({"configOptions": options})))
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(!acp.first_model_pending());
    assert!(acp.accepts_turn().is_ok());

    // A failed model switch cannot apply its effort to the previous model.
    let switch_to_wonder = || Command::SetModel {
        model: "muse-wonder-full-1.0".into(),
        reasoning: Some("xhigh".into()),
    };
    let step = acp.command(switch_to_wonder()).unwrap();
    let setting = only_write(&step);
    let step = acp
        .receive(json!({
            "jsonrpc": "2.0", "id": setting["id"],
            "error": {"code": -32602, "message": "Model switch refused"}
        }))
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(acp.accepts_turn().is_ok());

    let step = acp.command(switch_to_wonder()).unwrap();
    let setting = only_write(&step).clone();
    let step = acp
        .receive(reply(
            &setting,
            json!({"configOptions": muse_new()["configOptions"]}),
        ))
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(acp.accepts_turn().is_ok());
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    assert!(step.events.iter().any(|event| matches!(event,
        Event::ActiveModel(Some(model)) if model.id == "muse-wonder-full-1.0" && model.reasoning.is_none())));
}

#[test]
fn a_modern_model_switch_without_its_model_option_cannot_set_effort() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let step = acp
        .command(Command::SetModel {
            model: "gemini-3.8-flash".into(),
            reasoning: Some("low".into()),
        })
        .unwrap();
    let mut options = agy_new()["configOptions"].as_array().unwrap().clone();
    options.remove(0);
    let step = acp
        .receive(reply(only_write(&step), json!({"configOptions": options})))
        .unwrap();
    assert!(step.writes.is_empty(), "no confirmation of the destination");
    assert!(!acp.first_model_pending());
}

#[test]
fn an_unconfirmed_first_model_choice_retires_the_session_on_deadline() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    acp.command(Command::SetModel {
        model: "gemini-3.8-flash".into(),
        reasoning: Some("low".into()),
    })
    .unwrap();
    assert!(matches!(
        acp.metadata_wake(),
        Some(MetadataWake::RetryAt(_))
    ));
    acp.first_model_deadline = Some(Instant::now());
    let step = acp.refresh_metadata().unwrap();
    assert!(step.writes.is_empty());
    assert!(matches!(
        step.events.as_slice(),
        [Event::Ready(false), Event::Error { fatal: true, .. },]
    ));
    assert!(!acp.first_model_pending());
    assert!(acp.accepts_turn().is_err());
}

#[test]
fn an_agent_with_only_the_older_states_is_driven_by_their_own_methods() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let mut new = agy_new();
    new.as_object_mut().unwrap().remove("configOptions");
    acp.receive(reply(only_write(&opened), new)).unwrap();
    let step = acp
        .command(Command::SetModel {
            model: "gemini-3.8-flash-low".into(),
            reasoning: None,
        })
        .unwrap();
    let request = only_write(&step).clone();
    assert_eq!(request["method"], "session/set_model");
    assert_eq!(request["params"]["modelId"], "gemini-3.8-flash-low");
    let step = acp.receive(reply(&request, json!({}))).unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Models { selected: Some(model), .. } if model == "gemini-3.8-flash-low")));
    let step = acp
        .command(Command::SetPermissionMode {
            mode: "auto".into(),
        })
        .unwrap();
    let request = only_write(&step).clone();
    assert_eq!(request["method"], "session/set_mode");
    let step = acp.receive(reply(&request, Value::Null)).unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::PermissionMode(Some(mode)) if mode == "auto")));
    assert!(
        acp.command(Command::SetModel {
            model: "unknown".into(),
            reasoning: None
        })
        .is_err()
    );
}

#[test]
fn a_legacy_model_switch_attributes_reported_levels_to_the_new_model() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let reasoning = |level| {
        json!([{
            "id": "thought_level", "category": "thought_level", "type": "select",
            "currentValue": level, "options": [{"value": level}]
        }])
    };
    acp.receive(reply(
        only_write(&opened),
        json!({
            "sessionId": "fixture",
            "models": {"currentModelId": "first", "availableModels": [
                {"modelId": "first"}, {"modelId": "second"}
            ]},
            "configOptions": reasoning("high")
        }),
    ))
    .unwrap();
    let step = acp
        .command(Command::SetModel {
            model: "second".into(),
            reasoning: None,
        })
        .unwrap();
    let request = only_write(&step);
    assert_eq!(request["method"], "session/set_model");
    let step = acp
        .receive(reply(request, json!({"configOptions": reasoning("low")})))
        .unwrap();
    assert_eq!(
        levels(&step),
        vec![
            ("first".to_owned(), vec!["high".to_owned()]),
            ("second".to_owned(), vec!["low".to_owned()])
        ]
    );
    assert!(step.events.iter().any(|event| matches!(
        event, Event::Models { acp_reasoning_observed: Some(observed), .. }
            if observed == &["second"]
    )));
}

#[test]
fn streamed_text_and_thought_keep_one_item_each_until_a_tool_intervenes() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "hello");
    let mut ids = Vec::new();
    for frame in [
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Thinking "}}),
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "more"}}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "Hel"}}),
        json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": {"type": "text", "text": "lo"}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read", "kind": "read", "status": "pending"}),
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "After"}}),
    ] {
        for event in acp.receive(update(&session, frame)).unwrap().events {
            if let Event::TextDelta { id, thinking, .. } = event {
                ids.push((id, thinking));
            }
        }
    }
    assert_eq!(ids[0], ids[1]);
    assert!(ids[0].1);
    assert_eq!(ids[2], ("acp-message:m1".into(), false));
    assert_eq!(ids[2], ids[3]);
    assert_ne!(ids[4].0, ids[3].0);
    assert_ne!(ids[4].0, ids[0].0);
    // The same message resumed after another tool call continues below
    // the call, in an item of its own.
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "t2",
        "title": "Read", "kind": "read", "status": "pending"}),
    ))
    .unwrap();
    let mut resumed = Vec::new();
    for _ in 0..2 {
        for event in acp
            .receive(update(
                &session,
                json!({"sessionUpdate": "agent_message_chunk", "messageId": "m1",
                "content": {"type": "text", "text": "More"}}),
            ))
            .unwrap()
            .events
        {
            if let Event::TextDelta { id, .. } = event {
                resumed.push(id);
            }
        }
    }
    assert_eq!(resumed, ["acp-message:m1:2", "acp-message:m1:2"]);
}

#[test]
fn tool_calls_become_commands_and_diffs_become_file_changes() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "tools");
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "tool_call", "toolCallId": "run",
            "title": "List files", "kind": "execute", "status": "in_progress",
            "rawInput": {"command": ["ls", "-1"], "cwd": "/work/project"}}),
        ))
        .unwrap();
    assert!(matches!(&items(&step)[0].content, ItemContent::Command {
        command, label, action: CommandAction::Run, status: ToolStatus::Running, working_directory, ..
    } if command == "ls -1" && label == "List files" && working_directory == "/work/project"));
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "run",
            "status": "failed", "rawOutput": {"exitCode": 2},
            "content": [{"type": "content", "content": {"type": "text", "text": "ls: nope"}}]}),
        ))
        .unwrap();
    assert!(matches!(&items(&step)[0].content, ItemContent::Command {
        command, status: ToolStatus::Failed, exit_code: Some(2), output, ..
    } if command == "ls -1" && output == "ls: nope"));
    let step = acp
        .receive(update(&session, json!({"sessionUpdate": "tool_call", "toolCallId": "edit",
            "title": "Edit notes.md", "kind": "edit", "status": "completed",
            "content": [
                {"type": "diff", "path": "/work/project/notes.md", "oldText": "a\nb\n", "newText": "a\nc\n"},
                {"type": "diff", "path": "/work/project/new.md", "newText": "fresh\n"}]})))
        .unwrap();
    let ItemContent::Changes { changes, status } = &items(&step)[0].content else {
        panic!("{step:?}");
    };
    assert_eq!(*status, ToolStatus::Succeeded);
    assert_eq!(changes[0].kind, ChangeKind::Updated);
    assert_eq!(changes[0].diff, "@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
    assert_eq!(changes[1].kind, ChangeKind::Added);
    let step = acp
        .receive(update(&session, json!({"sessionUpdate": "tool_call", "toolCallId": "think",
            "title": "Plan the work", "kind": "think", "status": "completed", "rawInput": {"goal": "x"}})))
        .unwrap();
    assert!(
        matches!(&items(&step)[0].content, ItemContent::Tool { name, input, .. }
        if name == "Plan the work" && input.contains("\"goal\""))
    );
}

#[test]
fn an_update_before_its_call_or_a_late_copy_cannot_reopen_a_finished_step() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "tools");
    let step = acp
        .receive(update(&session, json!({"sessionUpdate": "tool_call_update", "toolCallId": "early",
            "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "done"}}]})))
        .unwrap();
    assert_eq!(items(&step)[0].id, "tool:early");
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "tool_call", "toolCallId": "early",
            "title": "Search", "kind": "search", "status": "pending",
            "rawInput": {"pattern": "TODO"}}),
        ))
        .unwrap();
    assert!(matches!(&items(&step)[0].content, ItemContent::Command {
        action: CommandAction::Search, status: ToolStatus::Succeeded, output, label, command, ..
    } if output == "done" && command == "Search" && label.is_empty()));
}

#[test]
fn a_permission_is_declined_by_its_kind_never_its_label() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "permission");
    let step = acp
        .receive(json!({"jsonrpc": "2.0", "id": "agent-7", "method": "session/request_permission",
            "params": {"sessionId": session,
                "toolCall": {"toolCallId": "edit", "title": "Edit config.toml", "kind": "edit", "status": "pending",
                    "content": [{"type": "diff", "path": "/work/project/config.toml", "oldText": "a\n", "newText": "b\n"}]},
                "options": [
                    {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                    // A refusal whose label reads like consent.
                    {"optionId": "allow", "name": "Allow (not really)", "kind": "reject_once"},
                    {"optionId": "never", "name": "Never", "kind": "reject_always"}]}}))
        .unwrap();
    let request = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Request(request) => Some(request.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.kind, RequestKind::Approval);
    assert_eq!(request.title, "Edit config.toml");
    assert_eq!(request.description, "/work/project/config.toml");
    assert_eq!(
        request
            .actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<Vec<_>>(),
        ["yes", "allow", "never"]
    );
    assert!(step.events.contains(&Event::Waiting(true)));
    assert!(matches!(
        &items(&step)[0].content,
        ItemContent::Changes {
            status: ToolStatus::Running,
            ..
        }
    ));
    assert!(!acp.answer_declines(&request.id, "yes"));
    assert!(acp.answer_declines(&request.id, "allow"));
    assert!(acp.answer_declines(&request.id, "never"));
    assert!(!acp.answer_declines("another-request", "never"));
    // An action no option has cancels the request, which declines it.
    assert!(acp.answer_declines(&request.id, "cancel"));
    assert!(
        acp.command(Command::Answer {
            request_id: request.id.clone(),
            action: "maybe".into(),
            answers: Vec::new(),
        })
        .is_err()
    );
    let step = acp
        .command(Command::Answer {
            request_id: request.id.clone(),
            action: "yes".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(
        only_write(&step),
        &json!({"jsonrpc": "2.0", "id": "agent-7", "result": {"outcome": {"outcome": "selected", "optionId": "yes"}}})
    );
    assert!(step.events.contains(&Event::RequestResolved {
        id: request.id.clone()
    }));
    assert!(step.events.contains(&Event::Waiting(false)));
    assert!(
        acp.command(Command::Answer {
            request_id: request.id,
            action: "yes".into(),
            answers: Vec::new(),
        })
        .is_err()
    );
}

#[test]
fn stopping_answers_open_approvals_cancelled_and_waits_for_the_prompt_reply() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let request = prompt(&mut acp, "permission");
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "run",
        "title": "Build", "kind": "execute", "status": "in_progress"}),
    ))
    .unwrap();
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "id": 41, "method": "session/request_permission",
            "params": {"sessionId": session, "toolCall": {"toolCallId": "run"},
                "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}}),
        )
        .unwrap();
    assert!(step.events.contains(&Event::Waiting(true)));
    assert!(!acp.outcome_pending());
    let step = acp.command(Command::Interrupt).unwrap();
    assert_eq!(
        step.writes,
        [
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
            json!({"jsonrpc": "2.0", "id": 41, "result": {"outcome": {"outcome": "cancelled"}}}),
        ]
    );
    assert!(step.events.contains(&Event::Waiting(false)));
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::RequestResolved { .. }))
    );
    assert!(acp.outcome_pending());
    // A second stop sends nothing more.
    assert!(acp.command(Command::Interrupt).unwrap().writes.is_empty());
    // Updates the agent sends before its reply still land.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "Stopping"}}),
        ))
        .unwrap();
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::TextDelta { .. }))
    );
    let step = acp
        .receive(reply(&request, json!({"stopReason": "cancelled"})))
        .unwrap();
    assert_eq!(finished(&step), Some(Kind::Cancelled));
    assert!(step.events.contains(&Event::Status {
        busy: false,
        detail: chat::TURN_INTERRUPTED.into(),
    }));
    assert!(step.events.contains(&Event::TurnSettled));
    assert!(items(&step).iter().any(|item| item.id == "tool:run"
        && matches!(
            item.content,
            ItemContent::Command {
                status: ToolStatus::Stopped,
                ..
            }
        )));
    assert!(!acp.outcome_pending());
    // The conversation takes the next prompt.
    prompt(&mut acp, "again");
}

#[test]
fn an_approval_asked_after_stopping_is_cancelled_without_being_shown() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let request = prompt(&mut acp, "permission");
    acp.command(Command::Interrupt).unwrap();
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "id": "late", "method": "session/request_permission",
            "params": {"sessionId": session, "toolCall": {"toolCallId": "edit", "title": "Edit"},
                "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}}),
        )
        .unwrap();
    assert_eq!(
        only_write(&step),
        &json!({"jsonrpc": "2.0", "id": "late", "result": {"outcome": {"outcome": "cancelled"}}})
    );
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Request(_) | Event::Waiting(true)))
    );
    // An agent that finished before it saw the stop reports its own outcome.
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(finished(&step), Some(Kind::Succeeded));
    assert!(!acp.outcome_pending());
}

#[test]
fn a_prompt_cancelled_as_a_request_is_still_the_stop_the_reader_asked_for() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(Command::Interrupt).unwrap();
    let step = acp
        .receive(refusal(&request, -32800, "Request cancelled"))
        .unwrap();
    assert_eq!(finished(&step), Some(Kind::Cancelled));
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    // Without a stop, the same reply is a failure.
    let request = prompt(&mut acp, "hold");
    let step = acp.receive(refusal(&request, -32800, "Cancelled")).unwrap();
    assert_eq!(finished(&step), Some(Kind::Failed));
}

#[test]
fn stopping_an_idle_conversation_sends_nothing() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let step = acp.command(Command::Interrupt).unwrap();
    assert!(step.writes.is_empty() && step.events.is_empty());
    assert!(!acp.outcome_pending());
}

#[test]
fn stop_reasons_settle_the_turn_with_their_outcome() {
    for (reason, outcome, notice) in [
        ("end_turn", Kind::Succeeded, false),
        ("max_tokens", Kind::Failed, true),
        ("max_turn_requests", Kind::Failed, true),
        ("refusal", Kind::Failed, true),
        ("a_future_reason", Kind::Failed, true),
    ] {
        let mut acp = adapter(&[], None);
        ready(&mut acp);
        let request = prompt(&mut acp, "hello");
        let step = acp
            .receive(reply(
                &request,
                json!({"stopReason": reason,
                "usage": {"totalTokens": 3, "inputTokens": 2, "outputTokens": 1}}),
            ))
            .unwrap();
        assert_eq!(finished(&step), Some(outcome), "{reason}");
        assert_eq!(
            step.events
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. })),
            notice,
            "{reason}"
        );
        assert!(step.events.contains(&Event::Status {
            busy: false,
            detail: "Ready".into()
        }));
        assert!(step.events.iter().any(|event| matches!(event,
            Event::Usage(usage) if usage.input_tokens == 2 && usage.output_tokens == 1)));
    }
}

#[test]
fn a_busy_or_unready_conversation_refuses_another_prompt() {
    let mut acp = adapter(&[], None);
    let send = |mode| Command::Prompt {
        text: "x".into(),
        attachments: Vec::new(),
        mode,
    };
    for mode in [PromptMode::Send, PromptMode::Steer, PromptMode::Queue] {
        assert!(acp.command(send(mode)).is_err(), "{mode:?}");
    }
    ready(&mut acp);
    let first = acp.command(send(PromptMode::Send)).unwrap();
    assert!(first.events.contains(&Event::TurnStarted));
    assert!(items(&first).iter().any(|item| item.role == Role::User
        && matches!(&item.content, ItemContent::Text { text, .. } if text == "x")));
    // A message written during the turn steers or queues; it never starts
    // a second turn.
    assert!(acp.command(send(PromptMode::Send)).is_err());
}

fn send(text: &str, mode: PromptMode) -> Command {
    Command::Prompt {
        text: text.into(),
        attachments: Vec::new(),
        mode,
    }
}

fn queue_listing(step: &Step) -> Option<Vec<(&str, &str)>> {
    step.events.iter().rev().find_map(|event| match event {
        Event::QueueChanged(messages) => Some(
            messages
                .iter()
                .map(|message| (message.id.as_str(), message.text.as_str()))
                .collect(),
        ),
        _ => None,
    })
}

fn prompt_texts(frame: &Value) -> Vec<&str> {
    frame["params"]["prompt"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn statuses(step: &Step) -> Vec<(bool, &str)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::Status { busy, detail } => Some((*busy, detail.as_str())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_message_queued_during_a_turn_waits_in_the_adapter() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    prompt(&mut acp, "hold");
    let step = acp
        .command(send("then run the tests", PromptMode::Queue))
        .unwrap();
    assert!(step.writes.is_empty(), "{:?}", step.writes);
    // Shown in the queue, not yet in the transcript.
    assert!(items(&step).is_empty());
    let listed = queue_listing(&step).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].0.starts_with("acp-queued:"));
    assert_eq!(listed[0].1, "then run the tests");
    let step = acp.command(send("and lint", PromptMode::Queue)).unwrap();
    let listed = queue_listing(&step).unwrap();
    assert_eq!(listed.len(), 2);
    let Some(Event::QueueChanged(messages)) = step
        .events
        .iter()
        .find(|event| matches!(event, Event::QueueChanged(_)))
    else {
        unreachable!()
    };
    assert!(
        messages
            .iter()
            .all(|message| message.editable && message.delivery == chat::QueueDelivery::NextTurn)
    );
}

#[test]
fn recovery_keeps_full_held_input_until_the_adapter_delivers_it() {
    let directory = tempfile::tempdir().unwrap();
    let attachment = chat::Attachment::file(directory.path().join("notes.txt"));
    std::fs::write(&attachment.path, "attachment content").unwrap();
    let text = format!("held {} tail", "雪".repeat(7 * 1024));
    for (profile, initialized, opened) in [
        (&antigravity::PROFILE, agy_initialize(), agy_new()),
        (&muse::PROFILE, muse_initialize(), muse_new()),
    ] {
        let mut acp = Acp {
            cwd: "/work/project".into(),
            ..Acp::new(profile)
        };
        let initialize = acp.initialize().unwrap();
        let open = acp
            .receive(reply(only_write(&initialize), initialized))
            .unwrap();
        let mut step = acp
            .receive(reply(only_write(&open), opened.clone()))
            .unwrap();
        while let Some(setting) = step.writes.first().cloned() {
            step = acp
                .receive(reply(
                    &setting,
                    json!({"configOptions": opened["configOptions"]}),
                ))
                .unwrap();
        }
        let current = prompt(&mut acp, "working");
        let step = acp
            .command(Command::Prompt {
                text: text.clone(),
                attachments: vec![attachment.clone()],
                mode: PromptMode::Queue,
            })
            .unwrap();
        let preview = queue_listing(&step).unwrap();
        assert_eq!(preview.len(), 1);
        assert!(preview[0].1.ends_with("[Queue preview truncated]"));
        let queued = acp.recovery_queue().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, preview[0].0);
        assert_eq!(queued[0].text, text);
        assert_eq!(queued[0].attachments, [attachment.clone()]);
        assert_eq!(queued[0].delivery, chat::QueueDelivery::NextTurn);
        assert!(queued[0].editable);

        // Delivery transfers the input out of the held queue. A removal
        // racing behind it cannot make that same input recoverable again.
        let delivered = acp
            .receive(reply(&current, json!({"stopReason": "end_turn"})))
            .unwrap();
        let next = delivered
            .writes
            .iter()
            .find(|frame| frame["method"] == "session/prompt")
            .unwrap();
        assert_eq!(prompt_texts(next), [text.as_str()]);
        let removal = acp
            .command(Command::RemoveQueued {
                id: queued[0].id.clone(),
            })
            .unwrap();
        assert!(
            removal
                .events
                .iter()
                .any(|event| matches!(event, Event::QueueRemoval(reply) if !reply.removed))
        );
        assert_eq!(acp.recovery_queue(), Some(Vec::new()));
    }
}

#[test]
fn the_prompt_reply_starts_everything_queued_in_the_same_step() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("first", PromptMode::Queue)).unwrap();
    acp.command(send("second", PromptMode::Queue)).unwrap();
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(finished(&step), Some(Kind::Succeeded));
    // The finished turn settles, and the next starts before the step ends:
    // the conversation is never idle in between, so nothing else can start
    // a turn of its own first.
    assert_eq!(statuses(&step), [(true, "Working")]);
    let next = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .expect("the queued prompt");
    assert_eq!(next["params"]["sessionId"], session.as_str());
    assert_eq!(prompt_texts(next), ["first", "second"]);
    let settled = step
        .events
        .iter()
        .position(|event| *event == Event::TurnSettled)
        .unwrap();
    let started = step
        .events
        .iter()
        .position(|event| *event == Event::TurnStarted)
        .unwrap();
    assert!(settled < started);
    // Each message reaches the transcript as it was written.
    let shown = items(&step)
        .into_iter()
        .filter(|item| item.role == Role::User)
        .filter_map(|item| match &item.content {
            ItemContent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(shown, ["first", "second"]);
    assert_eq!(queue_listing(&step), Some(Vec::new()));
    // The new turn ends as any other does.
    let step = acp
        .receive(reply(next, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(statuses(&step), [(false, "Ready")]);
    assert!(
        step.writes
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
}

#[test]
fn a_queued_message_can_be_removed_before_it_goes() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("keep", PromptMode::Queue)).unwrap();
    let step = acp.command(send("drop", PromptMode::Queue)).unwrap();
    let listed = queue_listing(&step).unwrap();
    let (kept, dropped) = (listed[0].0.to_owned(), listed[1].0.to_owned());
    let step = acp
        .command(Command::RemoveQueued {
            id: dropped.clone(),
        })
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(
        step.events
            .contains(&Event::QueueRemoval(chat::QueueRemoval {
                id: dropped.clone(),
                removed: true,
            }))
    );
    assert_eq!(queue_listing(&step), Some(vec![(kept.as_str(), "keep")]));
    // One that already left is answered as not removed.
    let step = acp
        .command(Command::RemoveQueued {
            id: dropped.clone(),
        })
        .unwrap();
    assert!(
        step.events
            .contains(&Event::QueueRemoval(chat::QueueRemoval {
                id: dropped,
                removed: false,
            }))
    );
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    let next = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .unwrap();
    assert_eq!(prompt_texts(next), ["keep"]);
}

#[test]
fn a_queue_waits_for_the_reader_after_a_refusal_to_work() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("later", PromptMode::Queue)).unwrap();
    let step = acp
        .receive(refusal(
            &request,
            rpc::AUTH_REQUIRED,
            "Authentication required",
        ))
        .unwrap();
    assert_eq!(statuses(&step), [(false, "Ready")]);
    assert!(
        step.writes
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
    // Still queued, it goes ahead of the reader's next message, in the
    // order written.
    assert_eq!(queue_listing(&step), None);
    let step = acp
        .command(send("signed in again", PromptMode::Send))
        .unwrap();
    assert_eq!(
        prompt_texts(only_write(&step)),
        ["later", "signed in again"]
    );
    assert_eq!(queue_listing(&step), Some(Vec::new()));
}

#[test]
fn a_stopped_turn_holds_its_queue_until_the_reader_starts_it() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("one", PromptMode::Queue)).unwrap();
    acp.command(send("two", PromptMode::Queue)).unwrap();
    let step = acp.command(send("three", PromptMode::Queue)).unwrap();
    let second = queue_listing(&step).unwrap()[1].0.to_owned();
    acp.command(Command::Interrupt).unwrap();
    let step = acp
        .receive(reply(&request, json!({"stopReason": "cancelled"})))
        .unwrap();
    // Nothing starts on its own after a Stop.
    assert_eq!(finished(&step), Some(Kind::Cancelled));
    assert_eq!(statuses(&step), [(false, chat::TURN_INTERRUPTED)]);
    assert!(
        step.writes
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
    // One started from the queue goes alone, and the rest follow it.
    let step = acp.command(Command::StartQueued { id: second }).unwrap();
    let started = only_write(&step).clone();
    assert_eq!(prompt_texts(&started), ["two"]);
    assert!(step.events.contains(&Event::TurnStarted));
    assert_eq!(queue_listing(&step).unwrap().len(), 2);
    let step = acp
        .receive(reply(&started, json!({"stopReason": "end_turn"})))
        .unwrap();
    let next = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .unwrap();
    assert_eq!(prompt_texts(next), ["one", "three"]);
}

#[test]
fn removing_what_a_stop_held_empties_the_queue_in_the_command_step() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    let step = acp.command(send("one", PromptMode::Queue)).unwrap();
    let held = queue_listing(&step).unwrap()[0].0.to_owned();
    acp.command(Command::Interrupt).unwrap();
    let step = acp
        .receive(reply(&request, json!({"stopReason": "cancelled"})))
        .unwrap();
    assert_eq!(statuses(&step), [(false, chat::TURN_INTERRUPTED)]);
    // No provider frame follows, so the runtime must announce from this
    // command step that the idle conversation now takes courier mail.
    let step = acp.command(Command::RemoveQueued { id: held }).unwrap();
    assert!(step.writes.is_empty());
    assert_eq!(queue_listing(&step), Some(Vec::new()));
}

#[test]
fn send_now_between_turns_starts_what_a_stop_held() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("one", PromptMode::Queue)).unwrap();
    acp.command(Command::Interrupt).unwrap();
    acp.receive(refusal(
        &request,
        rpc::REQUEST_CANCELLED,
        "Request cancelled",
    ))
    .unwrap();
    let step = acp.command(Command::SendQueuedNow).unwrap();
    assert_eq!(prompt_texts(only_write(&step)), ["one"]);
    assert!(step.events.contains(&Event::TurnStarted));
    assert_eq!(statuses(&step), [(true, "Working")]);
}

#[test]
fn a_queued_message_moved_ahead_during_a_turn_goes_first() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("one", PromptMode::Queue)).unwrap();
    let step = acp.command(send("two", PromptMode::Queue)).unwrap();
    let second = queue_listing(&step).unwrap()[1].0.to_owned();
    let step = acp.command(Command::StartQueued { id: second }).unwrap();
    assert!(step.writes.is_empty());
    let listed = queue_listing(&step).unwrap();
    assert_eq!(
        listed.iter().map(|(_, text)| *text).collect::<Vec<_>>(),
        ["two", "one"]
    );
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    let next = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .unwrap();
    assert_eq!(prompt_texts(next), ["two", "one"]);
}

/// A new process of the agent queued nothing: what the last one listed
/// went with it, and removing one of those is not answered as delivered.
#[test]
fn a_new_agent_process_starts_with_an_empty_queue_and_knows_no_earlier_one() {
    let mut earlier = adapter(&[], None);
    ready(&mut earlier);
    prompt(&mut earlier, "hold");
    let step = earlier.command(send("lost", PromptMode::Queue)).unwrap();
    let stale = queue_listing(&step).unwrap()[0].0.to_owned();

    let mut acp = adapter(&[], None);
    let step = acp.initialize().unwrap();
    assert_eq!(queue_listing(&step), Some(Vec::new()));
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    prompt(&mut acp, "hold");
    acp.command(send("fresh", PromptMode::Queue)).unwrap();
    assert!(
        acp.command(Command::RemoveQueued { id: stale.clone() })
            .is_err()
    );
    assert!(acp.command(Command::StartQueued { id: stale }).is_err());
    assert_eq!(acp.queued.len(), 1);
}

/// Antigravity 1.5.17, recorded: a second `session/prompt` sent while the
/// first one's command ran in the foreground. agy answered the first at
/// once and carried on under the second, the command still running.
#[test]
fn a_steered_message_takes_the_turn_over_until_the_last_reply() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let first = prompt(
        &mut acp,
        "Run `sleep 25 && date` in the foreground, then `ls`.",
    );
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "call_12838427222352362471",
            "name": "run_command", "status": "in_progress", "kind": "execute",
            "title": "sleep 25 && date",
            "rawInput": {"CommandLine": "sleep 25 && date", "WaitMsBeforeAsync": 30000}}),
    ))
    .unwrap();
    let step = acp
        .command(send("Also: what is 3+3?", PromptMode::Steer))
        .unwrap();
    let second = only_write(&step).clone();
    assert_eq!(second["method"], "session/prompt");
    assert_eq!(prompt_texts(&second), ["Also: what is 3+3?"]);
    assert!(items(&step).iter().any(|item| item.role == Role::User));
    assert!(!step.events.contains(&Event::TurnStarted));
    let step = acp
        .receive(reply(
            &first,
            json!({"stopReason": "end_turn", "usage": {"inputTokens": 12344, "outputTokens": 42}}),
        ))
        .unwrap();
    // Taken over, not ended: the command still runs under the second prompt.
    assert_eq!(finished(&step), None);
    assert!(statuses(&step).is_empty());
    assert!(!step.events.contains(&Event::TurnSettled));
    assert!(!items(&step).iter().any(|item| matches!(
        item.content,
        ItemContent::Command {
            status: ToolStatus::Stopped,
            ..
        }
    )));
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Usage(usage) if usage.input_tokens == 12344)));
    // The agent's updates still belong to the turn.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_12838427222352362471",
                "status": "completed", "rawOutput": {"output": "Wed 25 Nov", "exit_code": 0}}),
        ))
        .unwrap();
    assert!(items(&step).iter().any(|item| matches!(
        item.content,
        ItemContent::Command {
            status: ToolStatus::Succeeded,
            ..
        }
    )));
    let step = acp
        .receive(reply(
            &second,
            json!({"stopReason": "end_turn", "usage": {"inputTokens": 9289, "outputTokens": 195}}),
        ))
        .unwrap();
    assert_eq!(finished(&step), Some(Kind::Succeeded));
    assert_eq!(statuses(&step), [(false, "Ready")]);
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Usage(usage) if usage.input_tokens == 12344 + 9289)));
}

#[test]
fn send_now_steers_everything_queued_into_the_running_turn() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let first = prompt(&mut acp, "hold");
    acp.command(send("one", PromptMode::Queue)).unwrap();
    acp.command(send("two", PromptMode::Queue)).unwrap();
    let step = acp.command(Command::SendQueuedNow).unwrap();
    let steered = only_write(&step).clone();
    assert_eq!(prompt_texts(&steered), ["one", "two"]);
    assert_eq!(queue_listing(&step), Some(Vec::new()));
    assert_eq!(
        items(&step)
            .iter()
            .filter(|item| item.role == Role::User)
            .count(),
        2
    );
    // Nothing is left for the end of the turn to start.
    let step = acp
        .receive(reply(&first, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(step.writes.is_empty());
    let step = acp
        .receive(reply(&steered, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(
        step.writes
            .iter()
            .all(|frame| frame["method"] != "session/prompt")
    );
    assert_eq!(finished(&step), Some(Kind::Succeeded));
}

/// An agent stopped with two prompts open may answer only one of them;
/// the turn ends with the first that says it was cancelled.
#[test]
fn a_stopped_steered_turn_ends_with_its_first_cancelled_reply() {
    for newest_first in [false, true] {
        let mut acp = adapter(&[], None);
        ready(&mut acp);
        let first = prompt(&mut acp, "hold");
        let step = acp.command(send("more", PromptMode::Steer)).unwrap();
        let second = only_write(&step).clone();
        acp.command(Command::Interrupt).unwrap();
        // Steering a turn being stopped is refused; queueing is not.
        assert!(acp.command(send("again", PromptMode::Steer)).is_err());
        let (answered, late) = if newest_first {
            (&second, &first)
        } else {
            (&first, &second)
        };
        let step = acp
            .receive(reply(answered, json!({"stopReason": "cancelled"})))
            .unwrap();
        assert_eq!(finished(&step), Some(Kind::Cancelled), "{newest_first}");
        assert_eq!(statuses(&step), [(false, chat::TURN_INTERRUPTED)]);
        // The other's reply, if it ever comes, belongs to no turn.
        let step = acp
            .receive(refusal(late, rpc::REQUEST_CANCELLED, "Request cancelled"))
            .unwrap();
        assert!(step.events.is_empty(), "{newest_first}: {:?}", step.events);
        assert!(acp.command(send("next", PromptMode::Send)).is_ok());
    }
}

/// A call the reader refused that the agent never reports ended does not
/// keep a turn that waits on background work from saying so.
#[test]
fn a_refused_call_does_not_hide_a_wait_on_background_work() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "build in the background, then edit");
    let usage = |used: u64| {
        update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": used, "size": 1048576}),
        )
    };
    acp.receive(usage(100)).unwrap();
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "build", "kind": "execute",
            "title": "make release", "status": "in_progress"}),
    ))
    .unwrap();
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "The build runs in the background."}}),
    ))
    .unwrap();
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "id": "agent-9", "method": "session/request_permission",
            "params": {"sessionId": session,
                "toolCall": {"toolCallId": "edit", "title": "Edit notes.md", "kind": "edit",
                    "status": "pending"},
                "options": [
                    {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}}),
        )
        .unwrap();
    let request = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Request(request) => Some(request.id.clone()),
            _ => None,
        })
        .unwrap();
    acp.command(Command::Answer {
        request_id: request,
        action: "no".into(),
        answers: Vec::new(),
    })
    .unwrap();
    let step = acp.receive(usage(200)).unwrap();
    assert_eq!(statuses(&step), [(true, chat::WAITING_ON_BACKGROUND_WORK)]);
}

/// Antigravity 1.5.17, recorded: a turn whose command it started in the
/// background stays open until the command ends, the model silent after
/// saying it would wait.
#[test]
fn a_turn_held_open_by_background_commands_says_so() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let request = prompt(
        &mut acp,
        "Start `sleep 60` in the background and wait for it.",
    );
    let call = json!({"toolCallId": "call_12809436142955883336", "name": "run_command",
        "kind": "execute", "title": "sleep 60 && echo done-sleeping > slept.txt",
        "rawInput": {"CommandLine": "sleep 60 && echo done-sleeping > slept.txt",
            "NotificationTimeoutSeconds": 120, "WaitMsBeforeAsync": 500}});
    let with = |kind: &str, status: &str| {
        let mut frame = call.clone();
        frame["sessionUpdate"] = json!(kind);
        frame["status"] = json!(status);
        update(&session, frame)
    };
    let usage = |used: u64| {
        update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": used, "size": 1048576}),
        )
    };
    acp.receive(usage(12417)).unwrap();
    let step = acp.receive(with("tool_call", "in_progress")).unwrap();
    assert!(statuses(&step).is_empty());
    // The model goes on while the command runs: it runs in the background,
    // and is still shown running.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "I have started the background command. Waiting for it to complete."}}),
        ))
        .unwrap();
    assert!(
        items(&step)
            .iter()
            .all(|item| item.id != "tool:call_12809436142955883336")
    );
    assert!(statuses(&step).is_empty());
    // Its response over, the turn waits on the command alone.
    let step = acp.receive(usage(12644)).unwrap();
    assert_eq!(statuses(&step), [(true, chat::WAITING_ON_BACKGROUND_WORK)]);
    assert!(
        acp.receive(usage(12644))
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(event, Event::Status { .. }))
    );
    // The command ends and the agent is at work again.
    let step = acp.receive(with("tool_call_update", "completed")).unwrap();
    assert_eq!(statuses(&step), [(true, "Working")]);
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "\n\nFINISHED"}}),
        ))
        .unwrap();
    assert!(statuses(&step).is_empty());
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(statuses(&step), [(false, "Ready")]);
}

/// Only an agent whose commands are recorded going on in the background has
/// its turn read as waiting on them: Muse's model going on beside a command
/// says nothing of the kind.
#[test]
fn an_agent_without_background_commands_never_waits_on_them() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    acp.profile = &muse::PROFILE;
    prompt(&mut acp, "Build it.");
    let usage = |used: u64| {
        update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": used, "size": 1048576}),
        )
    };
    acp.receive(usage(10)).unwrap();
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "a", "kind": "execute",
            "title": "make", "status": "in_progress"}),
    ))
    .unwrap();
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "Building."}}),
    ))
    .unwrap();
    assert!(statuses(&acp.receive(usage(20)).unwrap()).is_empty());
}

#[test]
fn commands_made_side_by_side_are_not_taken_for_background_ones() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "two at once");
    acp.receive(update(
        &session,
        json!({"sessionUpdate": "usage_update", "used": 10, "size": 100}),
    ))
    .unwrap();
    for id in ["a", "b"] {
        let step = acp
            .receive(update(
                &session,
                json!({"sessionUpdate": "tool_call", "toolCallId": id, "kind": "execute",
                    "title": "make", "status": "in_progress"}),
            ))
            .unwrap();
        assert!(items(&step).iter().all(|item| !matches!(
            item.content,
            ItemContent::Command {
                status: ToolStatus::Waiting,
                ..
            }
        )));
        assert!(statuses(&step).is_empty());
    }
}

#[test]
fn agents_offer_the_queue_and_steer_only_where_recorded() {
    let capabilities = |acp: &Acp| match acp.capabilities() {
        Event::Capabilities(capabilities) => capabilities,
        _ => unreachable!(),
    };
    let agy = Acp::new(&antigravity::PROFILE);
    let offered = capabilities(&agy);
    assert!(offered.queue && offered.queue_remove && offered.queue_start);
    assert!(offered.steer && offered.send_queued_now);
    assert!(offered.interrupt_stops_background);
    let muse = Acp::new(&muse::PROFILE);
    let offered = capabilities(&muse);
    assert!(offered.queue && offered.queue_remove);
    assert!(!offered.steer && !offered.send_queued_now);
    assert!(!offered.interrupt_stops_background);
}

#[test]
fn an_agent_that_does_not_steer_queues_but_never_takes_a_message_mid_turn() {
    let mut acp = Acp {
        cwd: "/work/project".into(),
        ..Acp::new(&muse::PROFILE)
    };
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    acp.receive(reply(only_write(&opened), muse_new())).unwrap();
    assert!(acp.stage == Stage::Ready);
    let request = only_write(&acp.command(send("go", PromptMode::Send)).unwrap()).clone();
    assert!(acp.command(send("mid", PromptMode::Steer)).is_err());
    assert!(acp.command(Command::SendQueuedNow).is_err());
    acp.command(send("next", PromptMode::Queue)).unwrap();
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    let next = step
        .writes
        .iter()
        .find(|frame| frame["method"] == "session/prompt")
        .unwrap();
    assert_eq!(prompt_texts(next), ["next"]);
}

#[test]
fn a_resumed_conversation_replays_into_one_bounded_history() {
    let mut acp = adapter(&[], Some("01a1f155-1f9e-7081-b35f-88ead0a9187d"));
    let session = "01a1f155-1f9e-7081-b35f-88ead0a9187d";
    let initialize = acp.initialize().unwrap();
    let step = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let load = only_write(&step).clone();
    assert_eq!(load["method"], "session/load");
    assert_eq!(
        load["params"],
        json!({"sessionId": session, "cwd": "/work/project", "mcpServers": []})
    );
    assert!(step.events.contains(&Event::HistoryLoading(true)));
    let exchanges = history::MAX_ITEMS;
    for index in 0..exchanges {
        let question = match index {
            0 => "[Image 236]".into(),
            1 => "[cagents message from the user]\nFix mailbox-only work\n[Reply in a shell: cagents reply 0 \"…\" · seal 00000000000000000000000000000000]".into(),
            2 => "[Image 236] Investigate ACP history".into(),
            _ => format!("question {index}"),
        };
        for frame in [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": question}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("answer {index}")}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": " continued"}}),
        ] {
            let step = acp.receive(update(session, frame)).unwrap();
            assert!(step.events.is_empty(), "{:?}", step.events);
        }
    }
    acp.receive(update(
        session,
        json!({"sessionUpdate": "tool_call", "toolCallId": "t",
        "title": "Run", "kind": "execute", "status": "in_progress"}),
    ))
    .unwrap();
    acp.receive(update(
        session,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "t",
        "status": "completed"}),
    ))
    .unwrap();
    // State, rather than history, is published as it arrives.
    let step = acp
        .receive(update(
            session,
            json!({"sessionUpdate": "plan", "entries": [
            {"content": "Step", "priority": "high", "status": "completed"}]}),
        ))
        .unwrap();
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::Plan { .. }))
    );
    let step = acp.receive(reply(&load, muse_new())).unwrap();
    let [
        Event::OpeningTitle(title),
        Event::History { items, has_earlier },
        ..,
    ] = step.events.as_slice()
    else {
        panic!("{:?}", step.events);
    };
    assert_eq!(title.text(), "Investigate ACP history");
    assert!(!has_earlier);
    assert_eq!(items.len(), exchanges + 1);
    assert_eq!(items[0].id, history::TRUNCATED_ID);
    let last = &items[items.len() - 2];
    assert!(
        matches!(&last.content, ItemContent::Text { text, .. } if text == &format!("answer {} continued", exchanges - 1))
    );
    assert!(matches!(
        &items[items.len() - 1].content,
        ItemContent::Command {
            status: ToolStatus::Succeeded,
            ..
        }
    ));
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Session { id, .. } if id == session)));
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::Ready(true)))
    );
    // Live updates after the load are published as they arrive.
    let step = acp
        .receive(update(
            session,
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "live"}}),
        ))
        .unwrap();
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, Event::TextDelta { .. }))
    );
}

#[test]
fn a_replayed_message_keeps_its_attached_file_and_image() {
    let mut replay = history::Replay::default();
    let mut acp = adapter(&[], Some("s"));
    acp.session = Some("s".into());
    acp.replay = Some(replay);
    for frame in [
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Look"}}),
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo="}}),
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "resource_link", "uri": "file:///work/a%20b.txt", "name": "a b.txt"}}),
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Seen"}}),
    ] {
        acp.receive(update("s", frame)).unwrap();
    }
    replay = acp.replay.take().unwrap();
    let (items, opening_title) = replay.finish();
    assert!(opening_title.is_none(), "the full history fits");
    assert_eq!(items.len(), 3);
    assert!(
        matches!(&items[0].content, ItemContent::Text { text, .. } if text == "Look\nAttached file: /work/a b.txt")
    );
    assert!(
        matches!(&items[1].content, ItemContent::Image { path, reference: None, .. } if path == "data:image/png;base64,iVBORw0KGgo=")
    );
    assert_eq!(items[2].role, Role::Assistant);
}

#[test]
fn resuming_an_agent_that_cannot_load_fails_plainly() {
    let mut acp = adapter(&[], Some("abc"));
    let initialize = acp.initialize().unwrap();
    let mut capabilities = agy_initialize();
    capabilities["agentCapabilities"]["loadSession"] = json!(false);
    let step = acp
        .receive(reply(only_write(&initialize), capabilities))
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Error { message, fatal: true } if message.contains("cannot resume"))));
    assert!(!acp.initialization_pending());
}

#[test]
fn another_protocol_version_is_refused() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let mut version = agy_initialize();
    version["protocolVersion"] = json!(2);
    let step = acp
        .receive(reply(only_write(&initialize), version))
        .unwrap();
    assert!(step.writes.is_empty());
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Error { message, fatal: true } if message.contains("version 2"))));
}

#[test]
fn being_signed_out_of_antigravity_says_where_its_agents_read_the_sign_in() {
    let signed_out = |version: &str| {
        let mut launch = launch(&[], None);
        launch.provider = crate::agent::Provider::Antigravity;
        let mut acp = prepare(&antigravity::PROFILE, &launch).unwrap().1;
        let initialize = acp.initialize().unwrap();
        let mut initialized = agy_initialize();
        initialized["agentInfo"]["version"] = json!(version);
        let opened = acp
            .receive(reply(only_write(&initialize), initialized))
            .unwrap();
        let step = acp
            .receive(refusal(
                only_write(&opened),
                -32000,
                "Authentication required",
            ))
            .unwrap();
        match step.events.first() {
            Some(Event::Error { message, .. }) => message.clone(),
            events => panic!("{events:?}"),
        }
    };
    let current = signed_out("1.5.18");
    assert!(current.starts_with(SIGN_IN_PREFIX), "{current}");
    assert!(
        current.contains("sign in to Antigravity from cagents, then try again."),
        "{current}"
    );
    assert!(chat::requires_sign_in(
        crate::agent::Provider::Antigravity,
        &current
    ));
    // One that cannot keep its sign-in in a file is named, with the cause.
    let outdated = signed_out("1.5.15");
    assert!(outdated.starts_with(SIGN_IN_PREFIX), "{outdated}");
    assert!(
        outdated.contains("Antigravity 1.5.15 keeps its sign-in in the desktop keyring"),
        "{outdated}"
    );
}

#[test]
fn being_signed_out_is_a_recognisable_sign_in_failure() {
    let signed_out = |acp: &mut Acp, request: &Value| {
        acp.receive(refusal(request, -32000, "Authentication required"))
            .unwrap()
    };
    // While opening the conversation, it ends the conversation.
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let step = signed_out(&mut acp, only_write(&opened));
    let Some(Event::Error { message, fatal }) = step.events.first() else {
        panic!("{:?}", step.events);
    };
    assert!(*fatal);
    assert!(message.starts_with(SIGN_IN_PREFIX), "{message}");
    // The phrase the Codex sign-in recovery already recognises.
    assert!(chat::requires_sign_in(
        crate::agent::Provider::Codex,
        message
    ));
    // During a turn, it fails the turn and leaves the conversation open.
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "auth");
    let step = signed_out(&mut acp, &request);
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Error { message, fatal: false } if message.starts_with(SIGN_IN_PREFIX))));
    assert_eq!(finished(&step), Some(Kind::Failed));
    // Another error is reported in the agent's own words.
    let request = prompt(&mut acp, "x");
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603,
            "message": "Internal error", "data": {"details": "model overloaded"}}}),
        )
        .unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Error { message, fatal: false } if message == "Internal error: model overloaded")));
}

#[test]
fn a_turn_refused_for_no_allowance_keeps_the_provider_s_reason() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let refused = |acp: &mut Acp, error: Value| {
        let request = prompt(acp, "allowance");
        let step = acp
            .receive(json!({"jsonrpc": "2.0", "id": request["id"], "error": error}))
            .unwrap();
        assert_eq!(finished(&step), Some(Kind::Failed));
        step.events
            .iter()
            .find_map(|event| match event {
                Event::Error {
                    message,
                    fatal: false,
                } => Some(message.clone()),
                _ => None,
            })
            .unwrap()
    };
    // The reason is the most specific words the reply has.
    let message = refused(
        &mut acp,
        json!({"code": -32603, "message": "Internal error", "data": {"message":
            "model request failed: 402 Payment Required: this login has no subscription allowance"}}),
    );
    assert_eq!(
        message,
        format!(
            "{}model request failed: 402 Payment Required: this login has no subscription allowance",
            chat::ACP_NO_ALLOWANCE_PREFIX
        )
    );
    let provider = crate::agent::Provider::Muse;
    assert_eq!(
        chat::no_allowance_reason(provider, &message),
        Some(
            "model request failed: 402 Payment Required: this login has no subscription allowance"
        )
    );
    assert!(!chat::requires_sign_in(provider, &message));
    // Muse's reason code, wherever the reply puts it.
    let message = refused(
        &mut acp,
        json!({"code": -32603, "message": "No subscription allowance (no_allowance)",
            "data": {"reason": "no_allowance"}}),
    );
    assert_eq!(
        chat::no_allowance_reason(provider, &message),
        Some("No subscription allowance (no_allowance)")
    );
    // An allowance Muse has no figures for is not a refusal for one.
    let message = refused(
        &mut acp,
        json!({"code": -32603, "message": "Internal error", "data": {"message":
            "subscription allowance unavailable: the server sent no usage data"}}),
    );
    assert_eq!(chat::no_allowance_reason(provider, &message), None);
    // Words the agent only echoes, such as a request it failed to send,
    // are not its reason.
    let message = refused(
        &mut acp,
        json!({"code": -32603, "message": "Internal error", "data": {
            "message": "the tool call failed",
            "request": {"prompt": "why does the API answer 402 Payment Required?"}}}),
    );
    assert_eq!(chat::no_allowance_reason(provider, &message), None);
    // An HTTP status the agent reports is its own.
    let message = refused(
        &mut acp,
        json!({"code": -32603, "message": "Internal error",
            "data": {"status": 402, "message": "model request failed"}}),
    );
    assert_eq!(
        chat::no_allowance_reason(provider, &message),
        Some("model request failed")
    );
}

#[test]
fn file_terminal_and_unknown_requests_are_refused_and_unknown_frames_ignored() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    for (id, method) in [
        (json!(5), "fs/read_text_file"),
        (json!("w"), "fs/write_text_file"),
        (json!(6), "terminal/create"),
        (json!(7), "elicitation/create"),
    ] {
        let step = acp
            .receive(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"sessionId": session}}))
            .unwrap();
        let write = only_write(&step);
        assert_eq!(write["id"], id);
        assert_eq!(write["error"]["code"], -32601);
        assert!(step.events.is_empty());
    }
    for frame in [
        update(
            &session,
            json!({"sessionUpdate": "a_future_update", "content": {"x": 1}}),
        ),
        update(&session, json!({"content": {}})),
        update(
            "another-session",
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "not ours"}}),
        ),
        json!({"jsonrpc": "2.0", "method": "_vendor/notice", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 999, "result": {}}),
        json!({"something": "else"}),
        json!([1, 2, 3]),
    ] {
        let step = acp.receive(frame).unwrap();
        assert!(step.writes.is_empty() && step.events.is_empty());
    }
    let step = acp
        .receive(json!({"jsonrpc": "2.0", "id": 8, "method": "session/request_permission",
            "params": {"sessionId": "another-session", "toolCall": {"toolCallId": "x"}, "options": []}}))
        .unwrap();
    assert_eq!(only_write(&step)["error"]["code"], -32602);
}

#[test]
fn plans_commands_modes_titles_and_usage_are_published() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "plan", "entries": [
            {"content": "One", "priority": "high", "status": "completed"},
            {"content": "Two", "priority": "low", "status": "in_progress"},
            {"content": "Three", "priority": "low", "status": "pending"}]}),
        ))
        .unwrap();
    assert_eq!(
        step.events,
        [Event::Plan {
            steps: vec![
                PlanStep {
                    text: "One".into(),
                    status: PlanState::Done
                },
                PlanStep {
                    text: "Two".into(),
                    status: PlanState::Active
                },
                PlanStep {
                    text: "Three".into(),
                    status: PlanState::Pending
                },
            ]
        }]
    );
    let step = acp
        .receive(update(&session, json!({"sessionUpdate": "available_commands_update", "availableCommands": [
            {"name": "compact", "description": "Compact"}, {"name": "review", "description": "Review", "input": {"hint": "x"}}]})))
        .unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Commands(commands) if commands.len() == 2 && commands[0].name == "compact")));
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Capabilities(capabilities) if capabilities.compact)));
    let step = acp.command(Command::Compact).unwrap();
    assert_eq!(
        only_write(&step)["params"]["prompt"],
        json!([{"type": "text", "text": "/compact"}])
    );
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "auto"}),
        ))
        .unwrap();
    assert_eq!(step.events, [Event::PermissionMode(Some("auto".into()))]);
    let mut options = agy_new()["configOptions"].clone();
    options[0]["currentValue"] = json!("gemini-3.8-flash");
    options[1]["options"] = json!([{"value": "low", "name": "Low"}]);
    options[1]["currentValue"] = json!("low");
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "config_option_update", "configOptions": options}),
        ))
        .unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Models { models, selected: Some(selected), .. }
            if selected == "gemini-3.8-flash" && models[0].reasoning == ["low"])));
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "session_info_update", "title": "Fix the build"}),
        ))
        .unwrap();
    assert_eq!(
        step.events,
        [Event::Session {
            id: session.clone(),
            title: Some("Fix the build".into())
        }]
    );
    // Muse's name for a session it has not named is no name.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "session_info_update", "title": "New session"}),
        ))
        .unwrap();
    assert!(step.events.is_empty(), "{:?}", step.events);
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": 53000, "size": 200000,
            "cost": {"amount": 0.045, "currency": "USD"}}),
        ))
        .unwrap();
    let [Event::Usage(usage)] = step.events.as_slice() else {
        panic!("{:?}", step.events);
    };
    assert_eq!((usage.context_tokens, usage.context_limit), (53000, 200000));
    assert_eq!(usage.cost_microusd, Some(45000));
}

#[test]
fn antigravity_refreshes_the_exact_conversations_listed_title() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let sent = prompt(&mut acp, "Check the build");
    let ended = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    let list = only_write(&ended);
    assert_eq!(list["method"], "session/list");
    assert_eq!(list["params"], json!({"cwd": "/work/project"}));
    let missed = acp
        .receive(reply(
            list,
            json!({"sessions": [{"sessionId": "other", "title": "Another conversation"}],
                "nextCursor": "opaque-page"}),
        ))
        .unwrap();
    assert!(missed.events.is_empty());
    let next = only_write(&missed);
    assert_eq!(next["method"], "session/list");
    assert_eq!(next["params"]["cursor"], "opaque-page");
    let listed = |request: &Value, title: &str| {
        reply(
            request,
            json!({"sessions": [{"sessionId": session, "cwd": "/work/project", "title": title}]}),
        )
    };
    let titled = |title: &str| Event::Session {
        id: session.clone(),
        title: Some(title.into()),
    };
    assert_eq!(
        acp.receive(listed(next, "  Build setup  ")).unwrap().events,
        [titled("Build setup")]
    );
    let sent = prompt(&mut acp, "/rename Build diagnostics");
    let ended = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(
        acp.receive(listed(only_write(&ended), "Build diagnostics"))
            .unwrap()
            .events,
        [titled("Build diagnostics")]
    );
    // Antigravity's background summary updates can arrive after a turn has
    // settled and even after the saved metadata was read.
    let background = update(
        &session,
        json!({"sessionUpdate": "session_info_update", "title": "Build diagnostics report"}),
    );
    assert_eq!(
        acp.receive(background).unwrap().events,
        [titled("Build diagnostics report")]
    );
}

#[test]
fn post_turn_title_listing_stops_after_four_pages() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let sent = prompt(&mut acp, "Check the build");
    let mut step = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(only_write(&step)["params"], json!({"cwd": "/work/project"}));
    for page in 1..=4 {
        let request = only_write(&step);
        assert_eq!(request["method"], "session/list");
        step = acp
            .receive(reply(
                request,
                json!({"sessions": [{"sessionId": format!("other-{page}")}],
                    "nextCursor": format!("page-{}", page + 1)}),
            ))
            .unwrap();
        assert!(step.events.is_empty());
    }
    assert!(step.writes.is_empty());

    let sent = prompt(&mut acp, "/rename Build diagnostics");
    let ended = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    let request = only_write(&ended);
    assert_eq!(request["params"], json!({"cwd": "/work/project"}));
    assert_eq!(
        acp.receive(reply(
            request,
            json!({"sessions": [{"sessionId": session, "title": "Build diagnostics"}]}),
        ))
        .unwrap()
        .events,
        [Event::Session {
            id: session,
            title: Some("Build diagnostics".into()),
        }]
    );
}

#[test]
fn muse_reads_the_saved_title_and_prefers_later_title_notifications() {
    let session = muse_new()["sessionId"].as_str().unwrap().to_owned();
    let mut acp = adapter(&[], Some(&session));
    let init = acp.initialize().unwrap();
    let load = acp
        .receive(reply(only_write(&init), muse_initialize()))
        .unwrap();
    let mut listing = acp.receive(reply(only_write(&load), muse_new())).unwrap();
    // The saved conversation can be older than the post-turn search reaches.
    for page in 1..=4 {
        let request = only_write(&listing);
        assert_eq!(request["method"], "session/list");
        listing = acp
            .receive(reply(
                request,
                json!({"sessions": [{"sessionId": format!("other-{page}")}],
                    "nextCursor": format!("page-{}", page + 1)}),
            ))
            .unwrap();
    }
    let list = only_write(&listing);
    assert_eq!(list["method"], "session/list");
    assert_eq!(list["params"]["cursor"], "page-5");
    let listed = |request: &Value, title: &str| {
        reply(
            request,
            json!({"sessions": [{"sessionId": session, "cwd": "/work/project", "title": title}]}),
        )
    };
    let titled = |title: &str| Event::Session {
        id: session.clone(),
        title: Some(title.into()),
    };
    assert_eq!(
        acp.receive(listed(list, "Saved conversation"))
            .unwrap()
            .events,
        [titled("Saved conversation")]
    );
    let sent = prompt(&mut acp, "Continue the investigation");
    let ended = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    let notification = |session: &str, title: &str| {
        update(
            session,
            json!({"sessionUpdate": "session_info_update", "title": title}),
        )
    };
    assert!(
        acp.receive(notification("other", "Foreign title"))
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        acp.receive(notification(&session, "Updated investigation"))
            .unwrap()
            .events,
        [titled("Updated investigation")]
    );
    assert!(
        acp.receive(listed(only_write(&ended), "Stale title"))
            .unwrap()
            .events
            .is_empty()
    );
    let sent = prompt(&mut acp, "Confirm the fix");
    let ended = acp
        .receive(reply(&sent, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(ended.writes.is_empty(), "Muse publishes its title changes");
    assert_eq!(
        acp.receive(notification(&session, "Investigation complete"))
            .unwrap()
            .events,
        [titled("Investigation complete")]
    );
}

#[test]
fn a_correlated_run_is_started_scoped_settled_and_finished() {
    use crate::agent_execution::ProviderEvent;
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let run = crate::model::RunId::new();
    let step = acp
        .command_for_run(
            run,
            Command::Prompt {
                text: "x".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            },
        )
        .unwrap();
    assert!(
        matches!(&step.events[0], Event::Execution { run: started, event: ProviderEvent::Started { .. } } if *started == run)
    );
    let request = only_write(&step).clone();
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "y"}}),
        ))
        .unwrap();
    assert!(step.events.iter().any(|event| matches!(
        event,
        Event::Execution {
            event: ProviderEvent::Scope,
            ..
        }
    )));
    let step = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    let execution = step
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Execution { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        execution,
        [
            ProviderEvent::Settled,
            ProviderEvent::Finished {
                outcome: Kind::Succeeded
            },
            ProviderEvent::Scope,
        ]
    );
    assert!(
        acp.command_for_run(run, Command::Compact).is_err(),
        "only a direct prompt"
    );
}

/// A run started while a stop holds the reader's queue would send those
/// messages with its own prompt and answer for them; it waits instead.
#[test]
fn a_correlated_run_never_takes_the_readers_held_queue() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "hold");
    acp.command(send("mine", PromptMode::Queue)).unwrap();
    acp.command(Command::Interrupt).unwrap();
    acp.receive(reply(&request, json!({"stopReason": "cancelled"})))
        .unwrap();
    let run = crate::model::RunId::new();
    assert!(
        acp.command_for_run(run, send("the run's", PromptMode::Send))
            .is_err()
    );
    assert!(acp.turn.is_none());
    assert_eq!(acp.queued.len(), 1);
}

/// Whether the conversation last said, in a step, that it takes steering
/// and Send now.
fn steering(step: &Step) -> Option<(bool, bool)> {
    step.events.iter().rev().find_map(|event| match event {
        Event::Capabilities(capabilities) => {
            Some((capabilities.steer, capabilities.send_queued_now))
        }
        _ => None,
    })
}

/// A run's caller waits for its turn's answer, which a message steered in
/// would change: the message is queued for the next turn instead, and the
/// conversation says it takes no steering until the run is answered, so the
/// composer offers to queue rather than steer and no Send now.
#[test]
fn a_correlated_runs_turn_is_never_steered() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let run = crate::model::RunId::new();
    let started = acp
        .command_for_run(run, send("the run's", PromptMode::Send))
        .unwrap();
    assert_eq!(steering(&started), Some((false, false)));
    let request = only_write(&started).clone();
    let step = acp.command(send("mine", PromptMode::Steer)).unwrap();
    assert!(step.writes.is_empty(), "{:?}", step.writes);
    let refused = acp.command(Command::SendQueuedNow).unwrap_err();
    assert!(refused.contains("when the turn ends"), "{refused}");
    assert_eq!(acp.queued.len(), 1);
    assert_eq!(acp.turn.as_ref().unwrap().requests.len(), 1);
    // Answered, the run lets go: the queue starts the next turn, which
    // takes steering again.
    let ended = acp
        .receive(reply(&request, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(steering(&ended), Some((true, true)));
    assert!(acp.turn.as_ref().is_some_and(|turn| turn.run.is_none()));
}

/// A run the reader stops lets go of the conversation as one it answers
/// does: once the stopped turn ends, the conversation takes steering again,
/// and what was queued meanwhile waits for the reader.
#[test]
fn a_stopped_correlated_run_gives_steering_back() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let started = acp
        .command_for_run(
            crate::model::RunId::new(),
            send("the run's", PromptMode::Send),
        )
        .unwrap();
    let request = only_write(&started).clone();
    acp.command(send("mine", PromptMode::Steer)).unwrap();
    let stopping = acp.command(Command::Interrupt).unwrap();
    assert!(stopping.events.contains(&Event::Status {
        busy: true,
        detail: crate::chat::TURN_STOPPING.into(),
    }));
    // Still stopping, the turn answers the run and takes nothing steered.
    assert_eq!(steering(&stopping), None);
    let ended = acp
        .receive(reply(&request, json!({"stopReason": "cancelled"})))
        .unwrap();
    assert_eq!(steering(&ended), Some((true, true)));
    assert!(acp.turn.is_none());
    assert_eq!(acp.queued.len(), 1);
}

#[test]
fn an_image_is_sent_inline_only_to_an_agent_that_reads_them() {
    crate::chat::attachment_assets::with_test_image(|attachment| {
        let token = attachment.reference.as_ref().unwrap().token();
        let text = format!("What is in {token}?");
        let mut acp = adapter(&[], None);
        ready(&mut acp);
        let step = acp
            .command(Command::Prompt {
                text: text.clone(),
                attachments: vec![attachment.clone()],
                mode: PromptMode::Send,
            })
            .unwrap();
        let blocks = only_write(&step)["params"]["prompt"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0]["text"].as_str().unwrap().starts_with(&text));
        assert!(blocks[0]["text"].as_str().unwrap().contains("Image inputs"));
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert!(items(&step).iter().any(|item| matches!(&item.content,
            ItemContent::Image { reference: Some(reference), .. } if Some(reference) == attachment.reference.as_ref())));
        let mut acp = adapter(&[], None);
        ready(&mut acp);
        acp.images = false;
        let refused = acp
            .command(Command::Prompt {
                text,
                attachments: vec![attachment],
                mode: PromptMode::Send,
            })
            .unwrap_err();
        assert!(refused.contains("does not accept images"), "{refused}");
    });
}

#[test]
fn a_non_json_line_is_skipped_unless_it_hides_a_frame() {
    assert!(!rpc::embedded_frame(b"agy: warming up"));
    assert!(!rpc::embedded_frame(b"{\"jsonrpc\" mentioned in prose"));
    assert!(rpc::embedded_frame(
        br#"log: {"jsonrpc":"2.0","id":3,"result":{}} trailing"#
    ));
    let (spec, _) = create(&TEST_PROFILE, &launch(&["acp"], None)).unwrap();
    assert!(spec.stdout_noise.is_some());
    assert_eq!(spec.program, "/opt/fake-acp-agent");
    assert_eq!(spec.arguments, ["acp"]);
}

#[test]
fn a_launch_needs_an_absolute_directory_and_an_exact_session() {
    let mut relative = launch(&[], None);
    relative.recipe.working_directory = "project".into();
    assert!(create(&TEST_PROFILE, &relative).is_err());
    assert!(create(&TEST_PROFILE, &launch(&[], Some(""))).is_err());
    assert!(create(&TEST_PROFILE, &launch(&[], Some("a\nb"))).is_err());
    assert!(create(&TEST_PROFILE, &launch(&["--model"], None)).is_err());
}

#[test]
fn an_option_named_cancel_is_read_by_its_kind() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "permission");
    let step = acp
        .receive(
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/request_permission",
            "params": {"sessionId": session, "toolCall": {"toolCallId": "x"},
                "options": [{"optionId": "cancel", "name": "Continue", "kind": "allow_once"}]}}),
        )
        .unwrap();
    let Some(Event::Request(request)) = step
        .events
        .iter()
        .find(|event| matches!(event, Event::Request(_)))
    else {
        panic!("{:?}", step.events);
    };
    assert!(!acp.answer_declines(&request.id, "cancel"));
    let step = acp
        .command(Command::Answer {
            request_id: request.id.clone(),
            action: "cancel".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(
        only_write(&step)["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "cancel"})
    );
}

#[test]
fn an_approval_the_agent_withdraws_leaves_the_reader() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "permission");
    let ask = |acp: &mut Acp, id: Value| {
        acp.receive(
            json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
            "params": {"sessionId": session, "toolCall": {"toolCallId": "x"},
                "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}}),
        )
        .unwrap()
    };
    ask(&mut acp, json!("first"));
    ask(&mut acp, json!(2));
    let withdraw = |acp: &mut Acp, id: Value| {
        acp.receive(
            json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}}),
        )
        .unwrap()
    };
    let step = withdraw(&mut acp, json!("first"));
    assert_eq!(only_write(&step)["error"]["code"], -32800);
    assert_eq!(only_write(&step)["id"], "first");
    assert!(matches!(
        step.events.as_slice(),
        [Event::RequestResolved { .. }]
    ));
    // A request that is not open is not answered twice.
    assert!(withdraw(&mut acp, json!("first")).writes.is_empty());
    let step = withdraw(&mut acp, json!(2));
    assert!(step.events.contains(&Event::Waiting(false)));
    assert!(acp.permissions.is_empty());
}

#[test]
fn approvals_beyond_the_bound_are_cancelled_without_being_shown() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "permission");
    for id in 0..MAX_PERMISSIONS + 3 {
        let step = acp
            .receive(
                json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
                "params": {"sessionId": session, "toolCall": {"toolCallId": "x"},
                    "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}}),
            )
            .unwrap();
        let shown = step
            .events
            .iter()
            .any(|event| matches!(event, Event::Request(_)));
        assert_eq!(shown, id < MAX_PERMISSIONS, "{id}");
        if !shown {
            assert_eq!(
                only_write(&step)["result"]["outcome"]["outcome"],
                "cancelled"
            );
        }
    }
    assert_eq!(acp.permissions.len(), MAX_PERMISSIONS);
}

#[test]
fn modes_reach_the_reader_by_meaning_and_the_agent_in_its_own_words() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
    let modes = step.events.iter().find_map(|event| match event {
        Event::Capabilities(capabilities) => Some(capabilities.permission_modes.clone()),
        _ => None,
    });
    assert_eq!(
        modes.unwrap(),
        ["default", "acceptEdits", "auto", "plan"],
        "the profile's meaning replaces the agent's own identifier"
    );
    let session = acp.session.clone().unwrap();
    let step = acp
        .command(Command::SetPermissionMode {
            mode: "acceptEdits".into(),
        })
        .unwrap();
    let set = only_write(&step).clone();
    assert_eq!(set["method"], "session/set_config_option");
    assert_eq!(set["params"]["value"], "accept-edits");
    acp.receive(reply(
        &set,
        json!({"configOptions": agy_new()["configOptions"].clone()}),
    ))
    .unwrap();
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "accept-edits"}),
        ))
        .unwrap();
    assert!(step.events.iter().any(|event| matches!(event,
        Event::PermissionMode(Some(mode)) if mode == "acceptEdits")));
    // A mode the profile does not name keeps its identifier both ways.
    let step = acp
        .command(Command::SetPermissionMode {
            mode: "plan".into(),
        })
        .unwrap();
    assert_eq!(only_write(&step)["params"]["value"], "plan");
}

#[test]
fn models_the_profile_does_not_offer_are_never_listed_or_chosen() {
    let mut acp = adapter(&[], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let mut new = agy_new();
    new["configOptions"][0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"value": "gemini-4-neon-contributor", "name": "Shared"}));
    let step = acp.receive(reply(only_write(&opened), new)).unwrap();
    let models = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::Models { models, .. } => Some(models.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        models
            .iter()
            .all(|model| !model.id.ends_with("-contributor")),
        "{models:?}"
    );
    assert!(
        acp.command(Command::SetModel {
            model: "gemini-4-neon-contributor".into(),
            reasoning: None,
        })
        .is_err()
    );
}

#[test]
fn a_model_naming_its_reasoning_level_is_applied_as_both() {
    let mut acp = adapter(&["--model", "gemini-3.8-flash-low"], None);
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), agy_initialize()))
        .unwrap();
    let step = acp.receive(reply(only_write(&opened), agy_new())).unwrap();
    let model = only_write(&step);
    assert_eq!(model["params"]["configId"], "model");
    assert_eq!(model["params"]["value"], "gemini-3.8-flash");
    let mut options = agy_new()["configOptions"].clone();
    options[0]["currentValue"] = json!("gemini-3.8-flash");
    let step = acp
        .receive(reply(model, json!({"configOptions": options})))
        .unwrap();
    let level = only_write(&step);
    assert_eq!(level["params"]["configId"], "thought_level");
    assert_eq!(level["params"]["value"], "low");
}

#[test]
fn a_confirmed_identity_replaces_the_conversation_a_terminal_resumes() {
    use crate::agent::Provider;
    let mut arguments = ["--model", "m", "--conversation", "old", "--mode", "plan"]
        .map(String::from)
        .to_vec();
    align_resume_identity(Provider::Antigravity, &mut arguments, "new".into());
    assert_eq!(
        arguments,
        ["--model", "m", "--mode", "plan", "--conversation", "new"]
    );
    let mut arguments = ["--conversation=old"].map(String::from).to_vec();
    align_resume_identity(Provider::Antigravity, &mut arguments, "new".into());
    assert_eq!(arguments, ["--conversation", "new"]);
    let mut arguments = ["resume", "old", "--model", "m"].map(String::from).to_vec();
    align_resume_identity(Provider::Muse, &mut arguments, "new".into());
    assert_eq!(arguments, ["resume", "new", "--model", "m"]);
    // Neither the most recent conversation nor an option's value that reads
    // like an identity option survives or is taken for one.
    let mut arguments = ["--continue", "--agent", "--conversation", "-c"]
        .map(String::from)
        .to_vec();
    align_resume_identity(Provider::Antigravity, &mut arguments, "new".into());
    assert_eq!(
        arguments,
        ["--agent", "--conversation", "--conversation", "new"]
    );
}

/// Muse lists its modes starting with running everything unasked; the reader
/// is offered them in the order Claude's modes of the same meanings take.
#[test]
fn muse_modes_are_offered_in_the_order_of_claude_s_equivalents() {
    let mut acp = Acp {
        cwd: "/work/project".into(),
        ..Acp::new(&muse::PROFILE)
    };
    let initialize = acp.initialize().unwrap();
    let opened = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let mut new = muse_new();
    let modes = json!([
        {"name": "Allow all", "value": "allowAll"},
        {"name": "Ask before unmatched actions", "value": "promptUnmatched"},
        {"name": "Ask on request", "value": "onRequest"},
        {"name": "Deny unmatched actions", "value": "denyUnmatched"}]);
    new["configOptions"][0]["options"] = modes;
    let step = acp.receive(reply(only_write(&opened), new)).unwrap();
    let offered = step.events.iter().find_map(|event| match event {
        Event::Capabilities(capabilities) => Some(capabilities.permission_modes.clone()),
        _ => None,
    });
    assert_eq!(
        offered.unwrap(),
        ["manual", "auto", "dontAsk", "bypassPermissions"]
    );
}

// Antigravity 1.5.17's subagents, in the frames it sent for one started
// alone, two started together and one stopped mid-command.
const CHILD: &str = "8b1b5963-48e3-438e-8d6f-318ff25b3c04";
const OTHER_CHILD: &str = "389cf9b0-52ad-471d-8cb4-f458060dd84a";

fn invoke(call: &str, role: &str, prompt: &str, status: &str) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": call, "name": "invoke_subagent",
        "kind": "other", "title": "Run invoke_subagent", "status": status,
        "rawInput": {"Model": "inherit", "Prompt": prompt, "Role": role, "TypeName": "self"}})
}

fn child_command(child: &str, call: &str, line: &str, status: &str) -> Value {
    json!({"sessionUpdate": "tool_call", "toolCallId": format!("{child}:{call}"),
        "name": "run_command", "kind": "execute", "title": line, "status": status,
        "rawInput": {"CommandLine": line, "WaitMsBeforeAsync": 5000}})
}

fn asks(session: &str, id: &str, call: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {"sessionId": session, "toolCall": call, "options": [
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}})
}

/// Each subagent update: its ID, role, status and whether it waits.
fn listed(step: &Step) -> Vec<(&str, &str, Option<ToolStatus>, Option<bool>)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::Subagent {
                id,
                kind,
                status,
                waiting,
                ..
            } => Some((id.as_str(), kind.as_str(), *status, *waiting)),
            _ => None,
        })
        .collect()
}

/// Each subagent step shown: the subagent's ID, the step's ID and status.
fn child_steps(step: &Step) -> Vec<(&str, &str, ToolStatus)> {
    step.events
        .iter()
        .filter_map(|event| match event {
            Event::SubagentItem { id, item } => Some((
                id.as_str(),
                item.id.as_str(),
                match &item.content {
                    ItemContent::Command { status, .. } | ItemContent::Tool { status, .. } => {
                        *status
                    }
                    other => panic!("not a step: {other:?}"),
                },
            )),
            _ => None,
        })
        .collect()
}

fn request(step: &Step) -> Request {
    step.events
        .iter()
        .find_map(|event| match event {
            Event::Request(request) => Some(request.clone()),
            _ => None,
        })
        .expect("a request")
}

#[test]
fn a_subagent_is_listed_by_its_role_with_its_steps_apart_from_the_parents() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent list the files.");
    let call = "call_12362618081845022504";
    let subagent = format!("tool:{call}");
    let prompt_text = "Please list all the files and directories in the current working directory (/var/tmp/agy-probe-proj) and report the list back in your response.";
    let step = acp
        .receive(update(
            &session,
            invoke(call, "File Lister", prompt_text, "in_progress"),
        ))
        .unwrap();
    assert_eq!(
        listed(&step),
        [(
            subagent.as_str(),
            "File Lister",
            Some(ToolStatus::Running),
            Some(false)
        )]
    );
    assert!(step.events.iter().any(|event| matches!(event,
        Event::Subagent { description, model: None, background: Some(false), .. }
            if description == prompt_text)));
    // The call that started it stays among the parent's steps, under the
    // subagent's ID, which is how the step links to it.
    assert_eq!(items(&step)[0].id, subagent);
    // It reports its end as soon as the subagent starts.
    let step = acp
        .receive(update(
            &session,
            invoke(call, "File Lister", prompt_text, "completed"),
        ))
        .unwrap();
    assert!(listed(&step).is_empty());

    let command = child_command(
        CHILD,
        "call_6533409358328079481",
        "ls -la /var/tmp/agy-probe-proj",
        "pending",
    );
    let step = acp.receive(update(&session, command.clone())).unwrap();
    let step_id = format!("tool:{CHILD}:call_6533409358328079481");
    assert_eq!(
        child_steps(&step),
        [(subagent.as_str(), step_id.as_str(), ToolStatus::Running)]
    );
    assert!(items(&step).is_empty());
    assert!(listed(&step).is_empty());

    let step = acp.receive(asks(&session, "1", command.clone())).unwrap();
    let asked = request(&step);
    assert_eq!(asked.title, "File Lister: ls -la /var/tmp/agy-probe-proj");
    assert!(step.events.contains(&Event::Waiting(true)));
    assert_eq!(listed(&step), [(subagent.as_str(), "", None, Some(true))]);
    assert!(items(&step).is_empty());
    let step = acp
        .command(Command::Answer {
            request_id: asked.id,
            action: "allow".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(listed(&step), [(subagent.as_str(), "", None, Some(false))]);
    assert!(step.events.contains(&Event::Waiting(false)));

    let mut running = command.clone();
    running["sessionUpdate"] = json!("tool_call_update");
    running["status"] = json!("in_progress");
    acp.receive(update(&session, running)).unwrap();
    // The parent's response ending while the subagent's command runs leaves
    // the parent at work on its subagent, not waiting on background work.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": 12688, "size": 1048576}),
        ))
        .unwrap();
    assert!(statuses(&step).is_empty());
    assert!(child_steps(&step).is_empty());
    let mut done = command;
    done["sessionUpdate"] = json!("tool_call_update");
    done["status"] = json!("completed");
    let step = acp.receive(update(&session, done)).unwrap();
    assert_eq!(
        child_steps(&step),
        [(subagent.as_str(), step_id.as_str(), ToolStatus::Succeeded)]
    );

    assert!(
        acp.command(Command::StopSubagent {
            id: subagent.clone()
        })
        .is_err()
    );
    let Event::Capabilities(capabilities) = acp.capabilities() else {
        unreachable!()
    };
    assert!(!capabilities.stop_subagent);

    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(
        listed(&step),
        [(
            subagent.as_str(),
            "",
            Some(ToolStatus::Succeeded),
            Some(false)
        )]
    );
    let settled = step
        .events
        .iter()
        .position(|event| *event == Event::TurnSettled)
        .unwrap();
    let ended = step
        .events
        .iter()
        .position(|event| matches!(event, Event::Subagent { .. }))
        .unwrap();
    assert!(ended < settled, "the turn settles once its subagents have");
}

#[test]
fn subagents_started_together_are_told_apart_only_on_evidence() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Start two subagents together.");
    let patient = "call_14464107016637035427";
    let hasty = "call_14464107016637036432";
    for (call, role, text) in [
        (
            patient,
            "Patient Reader",
            "First read README.md, then run the shell command `sleep 6`, then run `echo patient-done`, and report the output.",
        ),
        (
            hasty,
            "Hasty Echo",
            "Run the shell command `echo hasty-done` at once using run_command. Then report back.",
        ),
    ] {
        acp.receive(update(&session, invoke(call, role, text, "in_progress")))
            .unwrap();
    }
    // The one started second asks first, for what only its prompt names.
    let step = acp
        .receive(update(
            &session,
            child_command(OTHER_CHILD, "call_1168", "echo hasty-done", "pending"),
        ))
        .unwrap();
    assert_eq!(child_steps(&step)[0].0, format!("tool:{hasty}"));
    let step = acp
        .receive(asks(
            &session,
            "1",
            child_command(OTHER_CHILD, "call_1168", "echo hasty-done", "pending"),
        ))
        .unwrap();
    assert_eq!(request(&step).title, "Hasty Echo: echo hasty-done");
    // `sleep 6` says too little to name a prompt, but one call is left.
    let step = acp
        .receive(update(
            &session,
            child_command(CHILD, "call_1209", "sleep 6", "pending"),
        ))
        .unwrap();
    assert_eq!(child_steps(&step)[0].0, format!("tool:{patient}"));
    acp.receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();

    // Two prompts that name nothing the child asks for leave it unmatched:
    // listed apart, under its own session, rather than under either role.
    let turn = prompt(&mut acp, "Start two more.");
    for (call, role) in [("call_1", "Reviewer"), ("call_2", "Tester")] {
        acp.receive(update(
            &session,
            invoke(
                call,
                role,
                "Look over the change and report.",
                "in_progress",
            ),
        ))
        .unwrap();
    }
    let unmatched = "0f4e7b52-9a1d-4c8e-b7a3-2d6f1e9c5a04";
    let command = child_command(unmatched, "call_3", "cargo test --workspace", "pending");
    let step = acp.receive(update(&session, command.clone())).unwrap();
    assert_eq!(
        listed(&step),
        [(unmatched, "", Some(ToolStatus::Running), Some(false))]
    );
    assert_eq!(child_steps(&step)[0].0, unmatched);
    assert!(items(&step).is_empty());
    let step = acp.receive(asks(&session, "2", command)).unwrap();
    assert_eq!(request(&step).title, "Subagent: cargo test --workspace");
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    let mut ended: Vec<_> = listed(&step)
        .into_iter()
        .filter_map(|(id, _, status, _)| status.map(|status| (id, status)))
        .collect();
    ended.sort_by_key(|(id, _)| *id);
    assert_eq!(
        ended,
        [
            (unmatched, ToolStatus::Succeeded),
            ("tool:call_1", ToolStatus::Succeeded),
            ("tool:call_2", ToolStatus::Succeeded),
        ]
    );
}

#[test]
fn subagents_reconcile_the_last_pair_after_an_ambiguous_child_asks_first() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Start two subagents together.");
    let mut snapshot = crate::chat::Snapshot::default();
    let mut receive = |acp: &mut Acp, frame| {
        let step = acp.receive(frame).unwrap();
        crate::chat::runtime::project_events_for_test(&mut snapshot, step.events);
    };
    for (id, role, task) in [
        ("review", "Reviewer", "Review the implementation."),
        ("test", "Tester", "Run `make test` and report."),
    ] {
        receive(
            &mut acp,
            update(&session, invoke(id, role, task, "completed")),
        );
    }
    let command = child_command(CHILD, "review-step", "git diff --stat", "pending");
    receive(&mut acp, asks(&session, "1", command.clone()));
    receive(
        &mut acp,
        update(
            &session,
            child_command(OTHER_CHILD, "test-step", "make test", "completed"),
        ),
    );
    receive(&mut acp, reply(&turn, json!({"stopReason": "end_turn"})));

    let [review, test] = snapshot.subagents.as_slice() else {
        panic!("expected two subagents, got {:?}", snapshot.subagents);
    };
    assert_eq!((&*review.id, &*review.kind), ("tool:review", "Reviewer"));
    assert_eq!(review.status, ToolStatus::Succeeded);
    assert!(!review.waiting);
    assert_eq!(review.items[0].id, format!("tool:{CHILD}:review-step"));
    assert_eq!((&*test.id, &*test.kind), ("tool:test", "Tester"));
    assert_eq!(test.items[0].id, format!("tool:{OTHER_CHILD}:test-step"));

    let mut later = command;
    later["status"] = json!("completed");
    let step = acp.receive(update(&session, later)).unwrap();
    assert_eq!(child_steps(&step)[0].0, "tool:review");
    assert!(listed(&step).is_empty());
}

#[test]
fn subagents_do_not_merge_a_child_with_a_call_started_after_its_first_step() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Start subagents in stages.");
    acp.receive(update(
        &session,
        child_command(CHILD, "early-step", "git diff --stat", "completed"),
    ))
    .unwrap();
    acp.receive(update(
        &session,
        invoke("late", "Tester", "Run the tests.", "completed"),
    ))
    .unwrap();
    let step = acp
        .receive(update(
            &session,
            child_command(OTHER_CHILD, "late-step", "make all", "completed"),
        ))
        .unwrap();
    assert_eq!(child_steps(&step)[0].0, "tool:late");

    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(
        !step
            .events
            .iter()
            .any(|event| matches!(event, Event::SubagentMerged { .. }))
    );
}

#[test]
fn a_step_of_a_subagent_no_call_started_never_joins_the_parents() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "Go on.");
    let step = acp
        .receive(update(
            &session,
            child_command(CHILD, "call_9", "make release", "in_progress"),
        ))
        .unwrap();
    assert_eq!(
        listed(&step),
        [(CHILD, "", Some(ToolStatus::Running), Some(false))]
    );
    assert_eq!(child_steps(&step)[0].0, CHILD);
    assert!(items(&step).is_empty());
    // A parent call ID with a colon in it is still the parent's.
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "tool_call", "toolCallId": "mcp:search",
                "title": "Search", "kind": "search", "status": "in_progress"}),
        ))
        .unwrap();
    assert_eq!(items(&step)[0].id, "tool:mcp:search");
    assert!(child_steps(&step).is_empty());
}

#[test]
fn a_refused_subagent_command_ends_its_wait() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    prompt(&mut acp, "Have a subagent build it.");
    let call = "call_7";
    let subagent = format!("tool:{call}");
    acp.receive(update(
        &session,
        invoke(call, "Builder", "Run make in the project.", "in_progress"),
    ))
    .unwrap();
    let command = child_command(CHILD, "call_8", "make all -j8", "pending");
    let step = acp.receive(asks(&session, "1", command.clone())).unwrap();
    assert_eq!(listed(&step), [(subagent.as_str(), "", None, Some(true))]);
    let step = acp
        .command(Command::Answer {
            request_id: request(&step).id,
            action: "deny".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(listed(&step), [(subagent.as_str(), "", None, Some(false))]);
    // The refused command is over though the agent never says so, and it
    // never held the parent's own turn.
    let mut running = command;
    running["sessionUpdate"] = json!("tool_call_update");
    running["status"] = json!("in_progress");
    acp.receive(update(&session, running)).unwrap();
    let step = acp
        .receive(update(
            &session,
            json!({"sessionUpdate": "usage_update", "used": 1, "size": 1048576}),
        ))
        .unwrap();
    assert!(statuses(&step).is_empty());

    // Stopping the turn while the subagent asks ends its wait as well.
    let step = acp
        .receive(asks(
            &session,
            "2",
            child_command(CHILD, "call_9", "make install", "pending"),
        ))
        .unwrap();
    assert_eq!(listed(&step), [(subagent.as_str(), "", None, Some(true))]);
    let step = acp.command(Command::Interrupt).unwrap();
    assert!(listed(&step).contains(&(subagent.as_str(), "", None, Some(false))));
}

#[test]
fn a_stopped_turn_stops_its_subagents_and_what_they_ran() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent sleep.");
    let call = "call_10804341966631667245";
    let subagent = format!("tool:{call}");
    acp.receive(update(
        &session,
        invoke(
            call,
            "Sleeper",
            "Run the shell command `sleep 45 && echo woke` in the foreground and report its exact stdout and stderr output back.",
            "in_progress",
        ),
    ))
    .unwrap();
    let command = child_command(
        "eb272a0d-ea69-44e7-9a89-2593529b6ff6",
        "call_5122787415558307460",
        "sleep 45 && echo woke",
        "in_progress",
    );
    acp.receive(update(&session, command.clone())).unwrap();
    acp.command(Command::Interrupt).unwrap();
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "cancelled"})))
        .unwrap();
    let step_id = format!("tool:{}", command["toolCallId"].as_str().unwrap());
    assert_eq!(
        child_steps(&step),
        [(subagent.as_str(), step_id.as_str(), ToolStatus::Stopped)]
    );
    assert_eq!(
        listed(&step),
        [(
            subagent.as_str(),
            "",
            Some(ToolStatus::Stopped),
            Some(false)
        )]
    );
    // agy fails the command after it has answered the prompt: still the
    // same subagent's, and still stopped.
    let mut failed = command;
    failed["sessionUpdate"] = json!("tool_call_update");
    failed["status"] = json!("failed");
    let step = acp.receive(update(&session, failed)).unwrap();
    assert!(listed(&step).is_empty());
    assert_eq!(
        child_steps(&step),
        [(subagent.as_str(), step_id.as_str(), ToolStatus::Stopped)]
    );
}

#[test]
fn a_resumed_conversation_lists_its_subagents_as_ended() {
    let session = "c25e32a9-5629-4617-889f-844d0916b3ff";
    let mut acp = adapter(&[], Some(session));
    let initialize = acp.initialize().unwrap();
    let step = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let load = only_write(&step).clone();
    assert_eq!(load["method"], "session/load");
    let call = "call_12362618081845022504";
    let subagent = format!("tool:{call}");
    let step = acp
        .receive(update(
            session,
            invoke(call, "File Lister", "List the files.", "completed"),
        ))
        .unwrap();
    assert_eq!(
        listed(&step),
        [(
            subagent.as_str(),
            "File Lister",
            Some(ToolStatus::Ended),
            Some(false)
        )]
    );
    // agy replays none of a subagent's steps. One that came could be any
    // replayed call's, so it is listed apart.
    let step = acp
        .receive(update(
            session,
            child_command(CHILD, "call_1", "ls -la /var/tmp/project", "completed"),
        ))
        .unwrap();
    assert_eq!(child_steps(&step)[0].0, CHILD);
    assert_eq!(
        listed(&step),
        [(CHILD, "", Some(ToolStatus::Ended), Some(false))]
    );
    let step = acp.receive(reply(&load, muse_new())).unwrap();
    let history = step
        .events
        .iter()
        .find_map(|event| match event {
            Event::History { items, .. } => Some(items),
            _ => None,
        })
        .unwrap();
    assert!(
        history.iter().all(|item| !item.id.contains(CHILD)),
        "{history:?}"
    );
    assert!(history.iter().any(|item| item.id == format!("tool:{call}")));
}

#[test]
fn only_a_session_id_before_the_colon_names_a_subagent() {
    assert_eq!(subagents::child_of(&format!("{CHILD}:call_1")), Some(CHILD));
    assert_eq!(subagents::child_of("call_6533409358328079481"), None);
    assert_eq!(subagents::child_of("mcp:search"), None);
    assert_eq!(subagents::child_of(&format!("{CHILD}:")), None);
}

/// A replayed call started no subagent of the turn that follows: a child
/// heard from then is not taken for it, however many calls were replayed.
#[test]
fn a_child_after_a_resume_is_not_taken_for_a_replayed_subagent() {
    let session = "c25e32a9-5629-4617-889f-844d0916b3ff";
    let mut acp = adapter(&[], Some(session));
    let initialize = acp.initialize().unwrap();
    let step = acp
        .receive(reply(only_write(&initialize), muse_initialize()))
        .unwrap();
    let load = only_write(&step).clone();
    acp.receive(update(
        session,
        invoke("call_1", "File Lister", "List the files.", "completed"),
    ))
    .unwrap();
    let mut step = acp.receive(reply(&load, muse_new())).unwrap();
    while let Some(setting) = step.writes.first().cloned() {
        let options = muse_new()["configOptions"].clone();
        step = acp
            .receive(reply(&setting, json!({"configOptions": options})))
            .unwrap();
    }
    prompt(&mut acp, "Go on.");
    let step = acp
        .receive(update(
            session,
            child_command(OTHER_CHILD, "call_2", "ls -la /var/tmp/project", "pending"),
        ))
        .unwrap();
    assert_eq!(child_steps(&step)[0].0, OTHER_CHILD);
    assert_eq!(
        listed(&step),
        [(OTHER_CHILD, "", Some(ToolStatus::Running), Some(false))]
    );
}

/// A subagent whose call failed, or that the reader would not let start,
/// never ran: it says so at once, is not settled as finished with the
/// turn, and no child is taken for it.
#[test]
fn a_subagent_that_never_started_ends_as_its_call_did() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have two subagents check it.");
    acp.receive(update(
        &session,
        invoke("call_1", "Checker", "Run make check.", "in_progress"),
    ))
    .unwrap();
    let mut failed = invoke("call_1", "Checker", "Run make check.", "failed");
    failed["sessionUpdate"] = json!("tool_call_update");
    failed.as_object_mut().unwrap().remove("name");
    let step = acp.receive(update(&session, failed)).unwrap();
    assert_eq!(
        listed(&step),
        [("tool:call_1", "", Some(ToolStatus::Failed), Some(false))]
    );

    // A call that reported its subagent started keeps it, whatever a late
    // revision says.
    acp.receive(update(
        &session,
        invoke("call_4", "Linter", "Run make lint.", "completed"),
    ))
    .unwrap();
    let mut late = invoke("call_4", "Linter", "Run make lint.", "failed");
    late["sessionUpdate"] = json!("tool_call_update");
    let step = acp.receive(update(&session, late)).unwrap();
    assert!(listed(&step).is_empty());

    // Asked about before it was reported, the call is known by its title,
    // and its subagent waits on the answer before it is listed.
    let mut asked = invoke("call_2", "Tester", "Run cargo test.", "pending");
    asked.as_object_mut().unwrap().remove("name");
    let step = acp.receive(asks(&session, "1", asked)).unwrap();
    assert!(listed(&step).is_empty());
    assert_eq!(request(&step).title, "Run invoke_subagent");
    let step = acp
        .command(Command::Answer {
            request_id: request(&step).id,
            action: "deny".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert_eq!(
        listed(&step),
        [(
            "tool:call_2",
            "Tester",
            Some(ToolStatus::Denied),
            Some(false)
        )]
    );

    let step = acp
        .receive(update(
            &session,
            child_command(CHILD, "call_3", "make check", "pending"),
        ))
        .unwrap();
    // The one subagent left that started is the child's.
    assert_eq!(child_steps(&step)[0].0, "tool:call_4");
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert_eq!(
        listed(&step),
        [("tool:call_4", "", Some(ToolStatus::Succeeded), Some(false))]
    );
}

/// A subagent's request still open when the turn ends no longer holds it
/// waiting.
#[test]
fn a_turn_that_ends_with_a_subagent_asking_releases_it() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent build it.");
    acp.receive(update(
        &session,
        invoke("call_1", "Builder", "Run make.", "in_progress"),
    ))
    .unwrap();
    let step = acp
        .receive(asks(
            &session,
            "1",
            child_command(CHILD, "call_2", "make all", "pending"),
        ))
        .unwrap();
    assert_eq!(listed(&step), [("tool:call_1", "", None, Some(true))]);
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(
        listed(&step)
            .iter()
            .all(|(_, _, _, waiting)| *waiting == Some(false))
    );
    assert!(step.events.contains(&Event::Waiting(false)));
}

/// Muse reports no subagents: a call ID with a session ID before its colon
/// is one of its own steps.
#[test]
fn an_agent_without_subagents_keeps_every_step() {
    let mut acp = adapter(&[], None);
    acp.profile = &muse::PROFILE;
    let session = ready(&mut acp);
    prompt(&mut acp, "Go on.");
    let step = acp
        .receive(update(
            &session,
            child_command(CHILD, "call_1", "make all", "in_progress"),
        ))
        .unwrap();
    assert!(listed(&step).is_empty());
    assert!(child_steps(&step).is_empty());
    assert_eq!(items(&step)[0].id, format!("tool:{CHILD}:call_1"));
}

/// A pending `invoke_subagent`, such as one awaiting the reader's approval,
/// lists its subagent only once it is under way, or once a child shows it
/// has started.
#[test]
fn a_pending_subagent_call_is_listed_once_it_starts() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent test it.");
    let pending = invoke("call_1", "Tester", "Run cargo test.", "pending");
    let step = acp.receive(update(&session, pending.clone())).unwrap();
    assert!(listed(&step).is_empty());
    let step = acp.receive(asks(&session, "1", pending.clone())).unwrap();
    assert!(listed(&step).is_empty());
    let step = acp
        .command(Command::Answer {
            request_id: request(&step).id,
            action: "allow".into(),
            answers: Vec::new(),
        })
        .unwrap();
    assert!(listed(&step).is_empty());
    let mut started = pending;
    started["sessionUpdate"] = json!("tool_call_update");
    started["status"] = json!("in_progress");
    started.as_object_mut().unwrap().remove("rawInput");
    let step = acp.receive(update(&session, started)).unwrap();
    assert_eq!(
        listed(&step),
        [(
            "tool:call_1",
            "Tester",
            Some(ToolStatus::Running),
            Some(false)
        )]
    );

    // A child heard from while its call still reads pending started it.
    acp.receive(update(
        &session,
        invoke("call_2", "Builder", "Run make all.", "pending"),
    ))
    .unwrap();
    let step = acp
        .receive(update(
            &session,
            child_command(CHILD, "call_3", "make all", "in_progress"),
        ))
        .unwrap();
    assert_eq!(
        listed(&step),
        [(
            "tool:call_2",
            "Builder",
            Some(ToolStatus::Running),
            Some(false)
        )]
    );
    assert_eq!(child_steps(&step)[0].0, "tool:call_2");

    // One still pending when the turn ends never started.
    acp.receive(update(
        &session,
        invoke("call_4", "Linter", "Run make lint.", "pending"),
    ))
    .unwrap();
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    assert!(
        listed(&step)
            .iter()
            .all(|(id, _, _, _)| *id != "tool:call_4")
    );
}

/// A subagent's request after its turn has ended does not show it waiting
/// again.
#[test]
fn an_ended_subagent_asking_is_not_shown_waiting() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent build it.");
    acp.receive(update(
        &session,
        invoke("call_1", "Builder", "Run make.", "in_progress"),
    ))
    .unwrap();
    acp.receive(update(
        &session,
        child_command(CHILD, "call_2", "make all", "in_progress"),
    ))
    .unwrap();
    acp.receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    let step = acp
        .receive(asks(
            &session,
            "1",
            child_command(CHILD, "call_3", "make install", "pending"),
        ))
        .unwrap();
    assert_eq!(request(&step).title, "Builder: make install");
    assert!(listed(&step).is_empty());
}

/// Stopping a turn while a subagent asks cancels the request, ends the
/// wait, and stops the subagent and its step with the turn.
#[test]
fn stopping_while_a_subagent_asks_stops_it() {
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Have a subagent build it.");
    acp.receive(update(
        &session,
        invoke("call_1", "Builder", "Run make.", "in_progress"),
    ))
    .unwrap();
    let command = child_command(CHILD, "call_2", "make all", "pending");
    let step = acp.receive(asks(&session, "41", command.clone())).unwrap();
    assert_eq!(listed(&step), [("tool:call_1", "", None, Some(true))]);
    let step = acp.command(Command::Interrupt).unwrap();
    assert_eq!(
        step.writes[1],
        json!({"jsonrpc": "2.0", "id": "41", "result": {"outcome": {"outcome": "cancelled"}}})
    );
    assert!(listed(&step).contains(&("tool:call_1", "", None, Some(false))));
    assert!(step.events.contains(&Event::Waiting(false)));
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "cancelled"})))
        .unwrap();
    assert_eq!(
        listed(&step),
        [("tool:call_1", "", Some(ToolStatus::Stopped), Some(false))]
    );
    let step_id = format!("tool:{CHILD}:call_2");
    assert!(
        child_steps(&step)
            .iter()
            .all(|(subagent, id, status)| *subagent == "tool:call_1"
                && *id == step_id
                && *status != ToolStatus::Running)
    );
}

/// Beyond the children and calls it keeps, the oldest are forgotten, and
/// a forgotten child heard from again stays with its subagent.
#[test]
fn subagents_beyond_the_bounds_forget_the_oldest() {
    use super::subagents::{MAX_CHILDREN, MAX_INVOCATIONS};
    let child = |n: usize| format!("00000000-0000-4000-8000-{n:012}");
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    let turn = prompt(&mut acp, "Start many subagents.");
    acp.receive(update(
        &session,
        invoke("call_0", "First", "Run make first.", "in_progress"),
    ))
    .unwrap();
    // The first child is the one unmatched call's; the rest are their own.
    for n in 0..=MAX_CHILDREN {
        acp.receive(update(
            &session,
            child_command(&child(n), "step", "make all", "in_progress"),
        ))
        .unwrap();
    }
    let step = acp
        .receive(update(
            &session,
            child_command(&child(0), "again", "make again", "in_progress"),
        ))
        .unwrap();
    assert!(listed(&step).is_empty());
    assert_eq!(child_steps(&step)[0].0, "tool:call_0");

    for n in 1..=MAX_INVOCATIONS {
        acp.receive(update(
            &session,
            invoke(&format!("call_{n}"), "Many", "Run it.", "in_progress"),
        ))
        .unwrap();
    }
    let step = acp
        .receive(reply(&turn, json!({"stopReason": "end_turn"})))
        .unwrap();
    let ended = listed(&step);
    assert_eq!(ended.len(), MAX_INVOCATIONS);
    assert!(
        ended
            .iter()
            .all(|(_, _, status, _)| *status == Some(ToolStatus::Succeeded))
    );
    assert!(
        ended
            .iter()
            .any(|(id, _, _, _)| *id == format!("tool:call_{MAX_INVOCATIONS}"))
    );
}

/// A turn refused for want of a sign-in leaves the conversation running.
/// Once signed in elsewhere, resuming lifts the failure without a provider
/// request, since the agent reads its sign-in again for the next turn; only
/// a recognized sign-in failure, between turns, is lifted.
#[test]
fn a_turn_refused_for_its_sign_in_resumes_once_signed_in() {
    let mut acp = adapter(&[], None);
    ready(&mut acp);
    let request = prompt(&mut acp, "Summarize");
    let refused = acp
        .receive(refusal(&request, -32000, "Authentication required"))
        .unwrap();
    assert_eq!(finished(&refused), Some(Kind::Failed));
    let failure = refused
        .events
        .iter()
        .find_map(|event| match event {
            Event::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("the turn reports its sign-in failure");
    assert!(failure.starts_with(SIGN_IN_PREFIX), "{failure}");
    let resume = |acp: &mut Acp, failure: &str| {
        acp.command(Command::ResumeAfterLogin {
            failure: failure.into(),
            expected: None,
        })
    };
    assert!(resume(&mut acp, "fixture: adapter crashed").is_err());
    let resumed = resume(&mut acp, &failure).unwrap();
    assert!(resumed.writes.is_empty());
    assert_eq!(
        resumed.events,
        vec![Event::LoginRecovered {
            failure: failure.clone()
        }]
    );
    // Not while a turn runs.
    prompt(&mut acp, "Again");
    assert!(resume(&mut acp, &failure).is_err());
    // Nor for an agent that doesn't read its sign-in again on each request.
    let mut other = adapter(&[], None);
    ready(&mut other);
    other.profile = &muse::PROFILE;
    assert!(resume(&mut other, &failure).is_err());
}

#[test]
fn tracked_compaction_distinguishes_acp_prompt_completion_from_compaction() {
    use crate::agent_execution::CompactionOutcome;
    let mut acp = adapter(&[], None);
    let session = ready(&mut acp);
    acp.receive(update(
        &session,
        json!({
            "sessionUpdate":"available_commands_update",
            "availableCommands":[{"name":"compact","description":"Compact context"}]
        }),
    ))
    .unwrap();
    let compact = only_write(&acp.tracked_compaction().unwrap()).clone();
    let step = acp
        .receive(reply(&compact, json!({"stopReason":"end_turn"})))
        .unwrap();
    assert!(matches!(
        step.compaction,
        Some(CompactionOutcome::Unconfirmed(_))
    ));
    let compact = only_write(&acp.tracked_compaction().unwrap()).clone();
    let step = acp
        .receive(refusal(&compact, -32603, "compaction refused"))
        .unwrap();
    assert!(
        matches!(step.compaction, Some(CompactionOutcome::Failed(reason)) if reason.contains("compaction refused"))
    );

    acp.tracked_compaction().unwrap();
    let steer = only_write(&acp.command(send("wait", PromptMode::Steer)).unwrap()).clone();
    acp.command(Command::Interrupt).unwrap();
    let cancelled = acp
        .receive(reply(&steer, json!({"stopReason":"cancelled"})))
        .unwrap();
    assert!(matches!(
        cancelled.compaction,
        Some(CompactionOutcome::Failed(_))
    ));
    assert!(!acp.compaction_pending());
}
