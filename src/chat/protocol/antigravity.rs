//! Antigravity CLI's documented stream-json mode: NDJSON events on stdin and
//! stdout, one process per conversation.
//!
//! What this mode cannot do shapes the adapter:
//! - It never asks. A tool that needs an approval it does not hold is denied,
//!   so a conversation's permission mode is chosen once, at launch.
//! - It has no cancel message. SIGINT is the only way to stop a turn and ends
//!   the process with it, so a new process continues with `--conversation`.
//! - Resuming replays nothing, so earlier messages are read from the
//!   conversation's `transcript.jsonl`. That file is undocumented, so one that
//!   cannot be read leaves what is already shown in place.
//! - Input is text alone, and the model is chosen once, at launch.
//!
//! Google's FAQ says wrapping `agy` inside other applications is not supported
//! on consumer plans, so [`create`] refuses a personal Google sign-in.

use super::{Adapter, ProcessSpec, Step};
use crate::agent_execution::ProviderEvent;
use crate::chat::{
    self, Command, CommandAction, Event, Item, ItemContent, Launch, Phase, PromptMode, Role,
    ToolStatus,
};
use crate::notifications::Kind;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const STREAMING: [&str; 4] = [
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
];

pub const CONSUMER_SIGN_IN: &str = "Use a terminal tab, or sign in with a Gemini API key or Google Cloud: Google does not support Antigravity CLI inside other apps on a personal sign-in.";

/// Whether the CLI signs in some way other than a personal Google account:
/// the API key provider, a Google Cloud project, or Application Default
/// Credentials. Only the settings' shape is read, never a credential.
pub fn business_sign_in(environment: &dyn Fn(&str) -> Option<String>, home: &Path) -> bool {
    let read = |path: PathBuf| {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or_default()
    };
    let settings = read(home.join(".gemini/antigravity-cli/settings.json"));
    settings["modelProvider"] == "gemini"
        || settings["gcp"].is_object()
        || environment("AGY_ADC_AUTH").as_deref() == Some("true")
        || read(home.join(".gemini/config/config.json"))["enableAdc"] == true
}

pub fn create(launch: &Launch) -> Result<(ProcessSpec, Box<dyn Adapter>), String> {
    let mut spec = ProcessSpec::from(&launch.recipe);
    let environment = |name: &str| {
        launch.recipe.environment.get(name).cloned().or_else(|| {
            (!launch.recipe.unset_environment.contains(name))
                .then(|| std::env::var(name).ok())
                .flatten()
        })
    };
    let home = environment("HOME").ok_or("Antigravity needs a home directory")?;
    if !business_sign_in(&environment, Path::new(&home)) {
        return Err(CONSUMER_SIGN_IN.into());
    }
    let mut kept = STREAMING.map(str::to_owned).to_vec();
    let mut mode = "default".to_owned();
    let mut model = None;
    let mut arguments = launch.recipe.arguments.iter();
    while let Some(argument) = arguments.next() {
        let (name, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        if matches!(name, "--input-format" | "--output-format") {
            if inline.is_none() {
                arguments.next();
            }
            continue;
        }
        match name {
            // The launch's session ID alone selects the conversation below.
            "--conversation" => {
                if inline.is_none() {
                    arguments.next();
                }
                continue;
            }
            "--continue" | "-c" => continue,
            "--dangerously-skip-permissions" => mode = "yolo".into(),
            _ => {}
        }
        let value = crate::agent::Provider::Antigravity.is_value_option(name);
        if !value && !crate::agent::Provider::Antigravity.is_switch(name) {
            return Err(format!(
                "Cannot safely transfer Antigravity launch option {name:?}; use an explicit resume launch without an initial prompt."
            ));
        }
        kept.push(argument.clone());
        if value && inline.is_none() {
            let next = arguments
                .next()
                .ok_or_else(|| format!("Missing value for {name}"))?;
            if name == "--mode" {
                mode.clone_from(next);
            } else if name == "--model" {
                model = Some(next.clone());
            }
            kept.push(next.clone());
        } else if name == "--mode" {
            mode = inline.unwrap_or_default().to_owned();
        } else if name == "--model" {
            model = inline.map(str::to_owned);
        }
    }
    let arguments = kept.clone();
    if let Some(id) = &launch.session_id {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err("Antigravity requires an exact conversation ID to resume.".into());
        }
        kept.extend(["--conversation".to_owned(), id.clone()]);
    }
    if !launch.recipe.working_directory.is_absolute() {
        return Err("Antigravity requires an absolute working directory.".into());
    }
    spec.arguments = kept;
    Ok((
        spec,
        Box::new(Antigravity {
            root: Path::new(&home).join(".gemini/antigravity-cli"),
            cwd: launch
                .recipe
                .working_directory
                .to_string_lossy()
                .into_owned(),
            resume: launch.session_id.clone(),
            mode,
            model,
            arguments,
            run_nonce: uuid::Uuid::new_v4().simple().to_string(),
            ..Antigravity::default()
        }),
    ))
}

#[derive(Default)]
struct Antigravity {
    root: PathBuf,
    cwd: String,
    resume: Option<String>,
    mode: String,
    model: Option<String>,
    /// The launch's arguments, without a conversation to continue.
    arguments: Vec<String>,
    run_nonce: String,
    conversation: Option<String>,
    busy: bool,
    interrupted: bool,
    /// The current process has been stopped and takes nothing more.
    ending: bool,
    /// Processes that have replaced the first, so the reply steps of each
    /// remain separate items and what is already shown stays in place.
    relaunches: u64,
    turn: u64,
    /// Text of the reply step being streamed: its index and what arrived.
    reply: Option<(u64, String)>,
    execution: Option<crate::model::RunId>,
    usage: chat::Usage,
}

/// The conversation's readable transcript, oldest first. `None` if it is
/// missing or holds nothing this adapter recognizes.
fn history(root: &Path, id: &str, cwd: &str) -> Option<Vec<Item>> {
    let path = root
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl");
    let text = std::fs::read_to_string(path).ok()?;
    let mut items = Vec::new();
    let mut last_call: Option<(u64, String, String)> = None;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let step = record["step_index"].as_u64().unwrap_or(items.len() as u64);
        let content = record["content"].as_str().unwrap_or_default();
        match record["type"].as_str() {
            Some("USER_INPUT") => {
                if let Some(request) = user_request(content) {
                    items.push(item(id, step, Role::User, text_content(request)));
                }
            }
            Some("PLANNER_RESPONSE") => {
                if !content.is_empty() {
                    items.push(item(id, step, Role::Assistant, text_content(content)));
                }
                last_call = record["tool_calls"][0]["name"].as_str().map(|name| {
                    (
                        step,
                        name.to_owned(),
                        record["tool_calls"][0]["args"].to_string(),
                    )
                });
            }
            Some("GENERIC") => {
                if let Some((_, name, input)) = last_call.take() {
                    let content = tool_content(&name, &input, content, ToolStatus::Succeeded, cwd);
                    items.push(item(id, step, Role::Assistant, content));
                }
            }
            _ => {}
        }
    }
    (!items.is_empty()).then_some(items)
}

/// What the person wrote, without the framing the CLI records around it.
fn user_request(content: &str) -> Option<&str> {
    let (_, rest) = content.split_once("<USER_REQUEST>")?;
    let (request, _) = rest.split_once("</USER_REQUEST>")?;
    Some(request.trim())
}

fn text_content(text: &str) -> ItemContent {
    ItemContent::Text {
        text: text.into(),
        thinking: false,
        phase: Phase::Final,
    }
}

fn item(conversation: &str, step: u64, role: Role, content: ItemContent) -> Item {
    Item {
        id: format!("agy:{conversation}:{step}"),
        role,
        content,
        started_at: chat::now_ms(),
        at: chat::now_ms(),
    }
}

fn tool_content(
    name: &str,
    input: &str,
    output: &str,
    status: ToolStatus,
    cwd: &str,
) -> ItemContent {
    if name == "run_command" {
        let parameters: Value = serde_json::from_str(input).unwrap_or_default();
        let command = parameters["CommandLine"].as_str().unwrap_or(input);
        return ItemContent::Command {
            label: String::new(),
            command: command.trim_matches('"').into(),
            working_directory: cwd.into(),
            action: CommandAction::Run,
            status,
            exit_code: None,
            duration_ms: None,
            output: super::terminal_output::display(output.to_owned()),
        };
    }
    ItemContent::Tool {
        name: name.into(),
        input: input.into(),
        output: output.into(),
        status,
    }
}

impl Antigravity {
    fn conversation(&self) -> &str {
        self.conversation.as_deref().unwrap_or_default()
    }

    fn opened(&mut self, frame: &Value, step: &mut Step) -> Result<(), String> {
        let id = frame["conversation_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Antigravity did not identify its conversation.")?
            .to_owned();
        if self.resume.as_ref().is_some_and(|resume| *resume != id) {
            return Err(
                "Antigravity opened a different conversation than the one requested.".into(),
            );
        }
        self.conversation = Some(id.clone());
        // A Reset forgets the session, so it has to come before naming it.
        if self.resume.is_some()
            && self.relaunches == 0
            && let Some(items) = history(&self.root, &id, &self.cwd)
        {
            step.events.push(Event::Reset {
                reason: "Loading this Antigravity conversation.".into(),
            });
            step.events.extend(items.into_iter().map(Event::Item));
        }
        step.events.push(Event::Session {
            id: id.clone(),
            title: None,
        });
        step.events.push(Event::Capabilities(chat::Capabilities {
            ..chat::Capabilities::default()
        }));
        // The event's own `permission_mode` reads the same whatever was chosen.
        step.events
            .push(Event::PermissionMode(Some(self.mode.clone())));
        if self.model.is_some() {
            step.events.push(Event::Models {
                models: Vec::new(),
                selected: self.model.clone(),
                reasoning: None,
            });
        }
        step.events.push(Event::Ready(true));
        step.events.push(Event::Status {
            busy: false,
            detail: "Ready".into(),
        });
        Ok(())
    }

    fn step_update(&mut self, update: &Value, step: &mut Step) {
        let index = update["step_index"].as_u64().unwrap_or_default();
        let done = update["state"] == "DONE";
        let conversation = format!("{}:{}", self.conversation(), self.relaunches);
        match update["step_type"].as_str() {
            Some("agent_response") => {
                let delta = update["text_delta"].as_str().unwrap_or_default();
                if self.reply.as_ref().is_none_or(|(reply, _)| *reply != index) {
                    self.reply = Some((index, String::new()));
                }
                let (_, text) = self.reply.as_mut().expect("set above");
                text.push_str(delta);
                let trimmed = if done { text.trim_end() } else { text.as_str() };
                if !trimmed.is_empty() {
                    let phase = if done {
                        Phase::Final
                    } else {
                        Phase::Commentary
                    };
                    step.events.push(Event::Item(item(
                        &conversation,
                        index,
                        Role::Assistant,
                        ItemContent::Text {
                            text: trimmed.into(),
                            thinking: false,
                            phase,
                        },
                    )));
                }
            }
            Some("tool") => {
                let info = &update["tool_info"];
                let name = update["tool_name"].as_str().unwrap_or("Tool");
                let status = if !done {
                    ToolStatus::Running
                } else if info["error"].is_null() {
                    ToolStatus::Succeeded
                } else {
                    ToolStatus::Failed
                };
                let output = info["output"]
                    .as_str()
                    .or_else(|| info["error"].as_str())
                    .unwrap_or_default();
                let content = tool_content(
                    name,
                    &info["parameters"].to_string(),
                    output,
                    status,
                    &self.cwd,
                );
                step.events.push(Event::Item(item(
                    &conversation,
                    index,
                    Role::Assistant,
                    content,
                )));
            }
            _ => {}
        }
    }

    fn finished(&mut self, result: &Value, step: &mut Step) {
        // A streaming session's `usage` is its running total.
        if let Some(input) = result["usage"]["input_tokens"].as_u64() {
            self.usage.input_tokens = input;
        }
        if let Some(output) = result["usage"]["output_tokens"].as_u64() {
            self.usage.output_tokens = output;
        }
        step.events.push(Event::Usage(self.usage.clone()));
        let status = result["status"].as_str().unwrap_or_default();
        let outcome = if self.interrupted
            || matches!(status, "CANCELED" | "INTERRUPTED")
            || result["finished_reason"] == "interrupted"
        {
            Kind::Interrupted
        } else if status == "SUCCESS" {
            Kind::Succeeded
        } else {
            Kind::Failed
        };
        if outcome == Kind::Failed {
            step.events.push(Event::Error {
                message: result["error"]
                    .as_str()
                    .filter(|error| !error.is_empty())
                    .unwrap_or("Antigravity could not finish this turn.")
                    .to_owned(),
                fatal: false,
            });
        }
        if let Some(denied) = result["denied_actions"]
            .as_array()
            .filter(|d| !d.is_empty())
        {
            let names: Vec<String> = denied
                .iter()
                .map(|action| {
                    action
                        .as_str()
                        .map_or_else(|| action.to_string(), str::to_owned)
                })
                .collect();
            step.events.push(Event::Error {
                message: format!(
                    "Antigravity refused {} because this chat cannot ask for approval. Allow it in the CLI's settings or choose another permission mode in a new chat.",
                    names.join(", ")
                ),
                fatal: false,
            });
        }
        self.busy = false;
        self.interrupted = false;
        self.reply = None;
        self.ending = outcome == Kind::Interrupted;
        step.events.push(Event::TurnSettled);
        if self.ending {
            step.events.push(Event::Ready(false));
        }
        step.events.push(Event::Status {
            busy: false,
            detail: if self.ending { "Stopping" } else { "Ready" }.into(),
        });
        if let Some(run) = self.execution.take() {
            step.events.push(Event::Execution {
                run,
                event: ProviderEvent::Settled,
            });
            step.events.push(Event::Execution {
                run,
                event: ProviderEvent::Finished { outcome },
            });
        }
        step.events.push(Event::TurnFinished {
            key: Some(format!("agy-request:{}:{}", self.run_nonce, self.turn)),
            outcome,
        });
    }
}

impl Adapter for Antigravity {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }

    fn command_for_run(
        &mut self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<Step, String> {
        if self.busy
            || self.execution.is_some()
            || !matches!(
                &command,
                Command::Prompt {
                    mode: PromptMode::Send,
                    ..
                }
            )
        {
            return Err("A correlated Antigravity run requires one idle direct prompt".into());
        }
        let mut step = self.command(command)?;
        self.execution = Some(run);
        step.events.insert(
            0,
            Event::Execution {
                run,
                event: ProviderEvent::Started {
                    key: format!("agy-request:{}:{}", self.run_nonce, self.turn),
                },
            },
        );
        Ok(step)
    }

    fn command(&mut self, command: Command) -> Result<Step, String> {
        match command {
            Command::Prompt {
                text,
                attachments,
                mode,
                ..
            } => {
                if !attachments.is_empty() {
                    return Err("Antigravity chats accept text only.".into());
                }
                if mode != PromptMode::Send || self.busy || self.ending {
                    return Err("Antigravity cannot take a message while it is working.".into());
                }
                self.conversation
                    .as_ref()
                    .ok_or("Antigravity is still starting.")?;
                self.busy = true;
                self.turn += 1;
                let mut step = Step::default();
                step.writes.push(json!({
                    "event":"user",
                    "message":{"role":"user","content":text},
                }));
                let id = format!("agy-user:{}:{}", self.run_nonce, self.turn);
                step.events.push(Event::TurnStarted);
                step.events.push(Event::Item(Item {
                    id,
                    role: Role::User,
                    content: text_content(&text),
                    started_at: chat::now_ms(),
                    at: chat::now_ms(),
                }));
                step.events.push(Event::Status {
                    busy: true,
                    detail: "Working".into(),
                });
                Ok(step)
            }
            Command::Interrupt => {
                let mut step = Step::default();
                if self.busy {
                    self.interrupted = true;
                    step.interrupt_process = true;
                }
                Ok(step)
            }
            _ => Err("Antigravity chats choose this when they start.".into()),
        }
    }

    fn relaunch_after_exit(&mut self) -> Option<(Vec<String>, Step)> {
        let id = self.conversation.clone()?;
        if !self.ending && !self.interrupted {
            return None;
        }
        let mut step = Step::default();
        // The process can end on the signal without reporting the turn.
        if self.busy {
            self.finished(&json!({"finished_reason":"interrupted"}), &mut step);
        }
        self.ending = false;
        self.resume = Some(id.clone());
        self.relaunches += 1;
        let mut arguments = self.arguments.clone();
        arguments.extend(["--conversation".to_owned(), id]);
        Some((arguments, step))
    }

    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Step::default();
        match frame["event"].as_str() {
            Some("init") => self.opened(&frame, &mut step)?,
            Some("step_update") => self.step_update(&frame["step_update"], &mut step),
            Some("result") => self.finished(&frame["result"], &mut step),
            _ => {}
        }
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Antigravity {
        Antigravity {
            root: "/nonexistent".into(),
            cwd: "/work".into(),
            mode: "default".into(),
            run_nonce: "n".into(),
            ..Antigravity::default()
        }
    }

    fn init() -> Value {
        json!({"event":"init","conversation_id":"5c09bb42-7e18-40d3-9545-ac00cd55e9e0","init":{"permission_mode":"request-review"}})
    }

    #[test]
    fn a_personal_sign_in_is_refused_and_others_are_told_apart() {
        let home = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        assert!(!business_sign_in(&none, home.path()));
        assert!(business_sign_in(
            &|name: &str| (name == "AGY_ADC_AUTH").then(|| "true".to_owned()),
            home.path()
        ));
        let settings = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(
            settings.join("settings.json"),
            r#"{"modelProvider":"gemini"}"#,
        )
        .unwrap();
        assert!(business_sign_in(&none, home.path()));
    }

    #[test]
    fn a_prompt_before_init_waits_and_after_it_is_one_frame() {
        let mut adapter = adapter();
        let prompt = || Command::Prompt {
            text: "hi".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        };
        assert!(adapter.command(prompt()).is_err());
        adapter.receive(init()).unwrap();
        let step = adapter.command(prompt()).unwrap();
        assert_eq!(step.writes[0]["event"], "user");
        assert_eq!(step.writes[0]["message"]["content"], "hi");
        assert!(adapter.command(prompt()).is_err(), "no queueing");
    }

    #[test]
    fn interrupting_signals_only_a_running_turn_and_reports_interrupted() {
        let mut adapter = adapter();
        adapter.receive(init()).unwrap();
        assert!(
            !adapter
                .command(Command::Interrupt)
                .unwrap()
                .interrupt_process
        );
        adapter.busy = true;
        assert!(
            adapter
                .command(Command::Interrupt)
                .unwrap()
                .interrupt_process
        );
        let step = adapter
            .receive(json!({"event":"result","result":{"status":"ERROR","finished_reason":"interrupted"}}))
            .unwrap();
        assert!(step.events.iter().any(|event| matches!(
            event,
            Event::TurnFinished {
                outcome: Kind::Interrupted,
                ..
            }
        )));
    }

    #[test]
    fn stopping_continues_the_conversation_in_a_new_process_without_reloading_it() {
        let mut adapter = adapter();
        adapter.arguments = vec!["--mode".into(), "plan".into()];
        adapter.receive(init()).unwrap();
        assert!(
            adapter.relaunch_after_exit().is_none(),
            "a crash is a crash"
        );
        let prompt = || Command::Prompt {
            text: "hi".into(),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        };
        adapter.command(prompt()).unwrap();
        adapter.command(Command::Interrupt).unwrap();
        adapter
            .receive(json!({"event":"result","result":{"status":"ERROR","finished_reason":"interrupted"}}))
            .unwrap();
        assert!(adapter.command(prompt()).is_err(), "the process is ending");

        let (arguments, _) = adapter.relaunch_after_exit().unwrap();
        assert_eq!(
            arguments,
            [
                "--mode",
                "plan",
                "--conversation",
                "5c09bb42-7e18-40d3-9545-ac00cd55e9e0"
            ]
        );
        let step = adapter.receive(init()).unwrap();
        assert!(
            !step
                .events
                .iter()
                .any(|event| matches!(event, Event::Reset { .. }))
        );
        assert!(
            step.events
                .iter()
                .any(|event| matches!(event, Event::Ready(true)))
        );
        adapter.command(prompt()).unwrap();
    }

    #[test]
    fn a_process_that_ends_on_the_signal_still_settles_the_turn() {
        let mut adapter = adapter();
        adapter.receive(init()).unwrap();
        adapter.busy = true;
        adapter.command(Command::Interrupt).unwrap();
        let (_, step) = adapter.relaunch_after_exit().unwrap();
        assert!(step.events.iter().any(|event| matches!(
            event,
            Event::TurnFinished {
                outcome: Kind::Interrupted,
                ..
            }
        )));
    }

    #[test]
    fn refused_tools_are_reported_rather_than_implied_to_have_run() {
        let mut adapter = adapter();
        adapter.receive(init()).unwrap();
        adapter.busy = true;
        let step = adapter
            .receive(json!({"event":"result","result":{"status":"SUCCESS","denied_actions":["run_command"]}}))
            .unwrap();
        assert!(step.events.iter().any(|event| matches!(
            event,
            Event::Error { message, .. } if message.contains("run_command")
        )));
    }

    #[test]
    fn resuming_keeps_what_is_shown_when_the_transcript_cannot_be_read() {
        let mut adapter = adapter();
        adapter.resume = Some("5c09bb42-7e18-40d3-9545-ac00cd55e9e0".into());
        let step = adapter.receive(init()).unwrap();
        assert!(
            !step
                .events
                .iter()
                .any(|event| matches!(event, Event::Reset { .. }))
        );
    }

    #[test]
    fn a_resume_names_its_conversation_after_resetting_the_view() {
        let root = tempfile::tempdir().unwrap();
        let id = "5c09bb42-7e18-40d3-9545-ac00cd55e9e0";
        let logs = root
            .path()
            .join("brain")
            .join(id)
            .join(".system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("transcript.jsonl"),
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"pong"}"#,
        )
        .unwrap();
        let mut adapter = adapter();
        adapter.root = root.path().into();
        adapter.resume = Some(id.into());
        let events = adapter.receive(init()).unwrap().events;
        let position = |found: fn(&Event) -> bool| events.iter().position(found).unwrap();
        assert!(
            position(|event| matches!(event, Event::Reset { .. }))
                < position(|event| matches!(event, Event::Session { .. }))
        );
    }

    #[test]
    fn history_shows_what_was_written_and_said_without_the_framing() {
        let root = tempfile::tempdir().unwrap();
        let logs = root.path().join("brain/id/.system_generated/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("transcript.jsonl"),
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nhello\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}"#, "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"pong"}"#, "\n",
            ),
        )
        .unwrap();
        let items = history(root.path(), "id", "/work").unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0].content, ItemContent::Text { text, .. } if text == "hello"));
        assert!(matches!(&items[1].content, ItemContent::Text { text, .. } if text == "pong"));
    }
}
