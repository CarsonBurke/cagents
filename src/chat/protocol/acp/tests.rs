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
    assert!(!capabilities.steer && !capabilities.queue && !capabilities.compact);
    // By meaning: the test profile's `acceptEdits` is agy's `accept-edits`.
    assert_eq!(
        capabilities.permission_modes,
        ["default", "acceptEdits", "auto", "plan"]
    );
    assert!(!acp.initialization_pending());
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
        "currentValue": "xhigh", "id": "thought_level", "name": "Reasoning effort",
        "options": [{"name": "Extra high", "value": "xhigh"}], "type": "select"}));
    let step = acp
        .receive(reply(&setting, json!({"configOptions": options})))
        .unwrap();
    // spark-1.2 offers the level, so it is still asked for.
    assert_eq!(only_write(&step)["params"]["configId"], "thought_level");
    let step = acp
        .receive(reply(only_write(&step), json!({"configOptions": options})))
        .unwrap();
    assert!(step.writes.is_empty());
    let step = acp
        .command(Command::SetModel {
            model: "muse-wonder-full-1.0".into(),
            reasoning: Some("xhigh".into()),
        })
        .unwrap();
    let setting = only_write(&step).clone();
    let step = acp
        .receive(reply(
            &setting,
            json!({"configOptions": muse_new()["configOptions"]}),
        ))
        .unwrap();
    assert!(step.writes.is_empty());
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
    assert!(acp.command(send(PromptMode::Send)).is_err());
    ready(&mut acp);
    let first = acp.command(send(PromptMode::Send)).unwrap();
    assert!(first.events.contains(&Event::TurnStarted));
    assert!(items(&first).iter().any(|item| item.role == Role::User
        && matches!(&item.content, ItemContent::Text { text, .. } if text == "x")));
    assert!(acp.command(send(PromptMode::Send)).is_err());
    assert!(acp.command(send(PromptMode::Steer)).is_err());
    assert!(acp.command(send(PromptMode::Queue)).is_err());
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
        for frame in [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": format!("question {index}")}}),
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
    let Some(Event::History { items, has_earlier }) = step
        .events
        .iter()
        .find(|event| matches!(event, Event::History { .. }))
    else {
        panic!("{:?}", step.events);
    };
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
    let items = replay.finish();
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
    let options = agy_new()["configOptions"].clone();
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
