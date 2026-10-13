//! A `cagents` invocation an agent ran, read back with the CLI's own grammar.
//!
//! Parsing with [`Cli`] rather than matching words means a transcript reads
//! `cagents send --file notes.md peer` exactly as the command did, flags,
//! aliases and all, and stops recognizing a form the moment the CLI does.

use super::*;
use crate::chat::coordination::{
    Act, Delegate, Kind, Outbound, OutboundKind, Outcome, Spawn, Text,
};
use clap::{CommandFactory, FromArgMatches};
use std::cell::RefCell;

thread_local! {
    /// The grammar, built once: building it is most of what a parse costs,
    /// and a transcript is read again on every change.
    static GRAMMAR: RefCell<clap::Command> = RefCell::new(Cli::command());
}

/// What `argv`, starting at the `cagents` word, does, `stdin` being what the
/// shell line shows it reading. `None` for a command the CLI would reject.
pub(crate) fn coordination(argv: &[String], stdin: Text) -> Option<Kind> {
    let cli = GRAMMAR.with_borrow_mut(|grammar| {
        let matches = grammar.try_get_matches_from_mut(argv).ok()?;
        Cli::from_arg_matches(&matches).ok()
    })?;
    // As the CLI's `read_text` reads it: standard input only for a lone `-`.
    let text = |inline: Option<String>, file: Option<PathBuf>| match (inline, file) {
        (Some(inline), _) if inline == "-" => stdin.clone(),
        (Some(inline), _) => Text::Inline(inline),
        (None, Some(path)) if path.as_os_str() == "-" => stdin.clone(),
        (None, Some(path)) => Text::File(path.display().to_string()),
        (None, None) => Text::Unseen,
    };
    let message = |kind, to: String, body: messages::MessageArgs, waits| {
        Kind::Message(Outbound {
            kind,
            to,
            body: text(body.text, body.file),
            summary: String::new(),
            waits,
        })
    };
    let prompt = |prompt: agents::PromptArgs| match prompt.flag {
        Some(flag) => Text::Inline(flag),
        None => text(prompt.text, prompt.prompt_file),
    };
    Some(match cli.command {
        TopCommand::Send(send) => message(OutboundKind::Send, send.to, send.message, false),
        TopCommand::Ask(ask) => message(OutboundKind::Ask, ask.send.to, ask.send.message, true),
        TopCommand::Reply(reply) => {
            message(OutboundKind::Reply, reply.id, reply.message, reply.wait)
        }
        TopCommand::Inbox(inbox) => Kind::Act(Act::Inbox { waits: inbox.wait }),
        TopCommand::Sessions(_) => Kind::Act(Act::Sessions),
        TopCommand::Whoami => Kind::Act(Act::Whoami),
        TopCommand::Linear(linear) => {
            let (verb, subject) = linear.describe();
            Kind::Act(Act::Linear { verb, subject })
        }
        TopCommand::Agent { command } => match command {
            AgentCommand::Spawn {
                prompt: given,
                name,
                provider,
                directory,
                model,
                reasoning_effort,
                channel,
                wait,
                ..
            } => Kind::Spawn(Spawn {
                agents: vec![Delegate {
                    name: name.unwrap_or_default(),
                    kind: String::new(),
                    description: String::new(),
                    prompt: prompt(given),
                }],
                brief: String::new(),
                provider: provider
                    .map(|provider| provider.executable().to_owned())
                    .unwrap_or_default(),
                model: model.unwrap_or_default(),
                effort: reasoning_effort.unwrap_or_default(),
                directory: directory
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_default(),
                channel,
                placement: channel.map(crate::agent_execution::Placement::Channel),
                waits: wait.wait,
            }),
            AgentCommand::Send {
                agent,
                prompt: given,
                wait,
                ..
            } => Kind::Message(Outbound {
                kind: OutboundKind::Prompt,
                to: agent,
                body: prompt(given),
                summary: String::new(),
                waits: wait.wait,
            }),
            AgentCommand::Status { agent, .. } => Kind::Act(Act::Status { agent }),
            AgentCommand::List => Kind::Act(Act::Agents),
            AgentCommand::Wait { handles, .. } => Kind::Act(Act::Wait { agents: handles }),
            AgentCommand::Requests { agent } => Kind::Act(Act::Requests { agent }),
            AgentCommand::Answer {
                agent,
                request,
                action,
                ..
            } => Kind::Act(Act::Answer {
                agent,
                request,
                action,
            }),
            AgentCommand::Interrupt { agent, .. } => Kind::Act(Act::Interrupt { agent }),
            AgentCommand::Close { agent } => Kind::Act(Act::Close {
                agents: vec![agent],
            }),
            AgentCommand::Console { .. } => return None,
        },
        // The rest manage the application rather than other agents, and are
        // named by the subcommand words the agent typed.
        _ => Kind::Act(Act::Cli(subcommand_path(argv))),
    })
}

/// What a command's `output` says about the step `kind` describes.
pub(crate) fn outcome(kind: &Kind, output: &str) -> Outcome {
    if let Some(outcome) = json_outcome(kind, output) {
        return outcome;
    }
    Outcome {
        // A failure prints as `cagents: …` just before the command exits.
        // So do an ask's sent line, when its wait times out, and notices
        // about the CLI's own diagnostics, which say nothing of the step.
        error: output
            .lines()
            .rev()
            .filter(|line| messages::read_sent_line(line).is_none())
            .filter_map(|line| line.strip_prefix("cagents: "))
            .find(|line| {
                ![
                    crate::diagnostics::GAP_NOTICE,
                    crate::diagnostics::FAILURE_NOTICE,
                ]
                .contains(line)
            })
            .map(str::to_owned),
        ..read(kind, output)
    }
}

/// A successful launch reply is authoritative about resolved defaults. Human
/// output only identifies a provider after waiting; model/channel stay unknown.
pub(crate) fn enrich(kind: &mut Kind, outcome: &Outcome) {
    let (Kind::Spawn(spawn), Some(run)) = (kind, &outcome.run) else {
        return;
    };
    if !run.provider.is_empty() {
        spawn.provider.clone_from(&run.provider);
    }
    if !run.model.is_empty() {
        spawn.model.clone_from(&run.model);
    }
    if !run.effort.is_empty() {
        spawn.effort.clone_from(&run.effort);
    }
    if let Some(placement) = &run.placement {
        spawn.placement = Some(placement.clone());
        spawn.channel = run.channel;
    } else if run.channel.is_some() {
        spawn.channel = run.channel;
    }
}

/// Recognize only the CLI's complete envelope, not arbitrary JSON in an
/// agent's result or a later command's output. Typed wire objects preserve the
/// same IDs, enum values and optional defaults as the CLI's own replies.
fn json_outcome(kind: &Kind, output: &str) -> Option<Outcome> {
    let envelope: Value = serde_json::from_str(output.trim()).ok()?;
    if !envelope["ok"].as_bool()? {
        let message = envelope["error"]["message"].as_str()?;
        // Timeout errors can include a sent receipt even though waiting failed.
        let mut result = read(kind, message);
        result.error = Some(message.to_owned());
        return Some(result);
    }
    let expected = match kind {
        Kind::Spawn(_) => "agent.spawn",
        Kind::Message(message) => match message.kind {
            OutboundKind::Prompt => "agent.send",
            OutboundKind::Send => "send",
            OutboundKind::Ask => "ask",
            OutboundKind::Reply => "reply",
            OutboundKind::Report => return None,
        },
        Kind::Act(Act::Status { .. }) => "agent.status",
        Kind::Act(Act::Wait { .. }) => "agent.wait",
        Kind::Act(Act::Inbox { .. }) => "inbox",
        _ => return None,
    };
    if envelope["command"].as_str()? != expected {
        return None;
    }
    let data = &envelope["data"];
    let run = data
        .get("agent")
        .zip(data.get("run"))
        .and_then(|(agent, run)| {
            let agent: crate::agent_execution::Agent =
                serde_json::from_value(agent.clone()).ok()?;
            let run: crate::agent_execution::Run = serde_json::from_value(run.clone()).ok()?;
            if run.agent != agent.id {
                return None;
            }
            let channel = agent.channel();
            let placement = Some(agent.placement.clone());
            Some(crate::chat::coordination::Run {
                agent: crate::agent_execution::mailbox::short(agent.id),
                provider: agent.spec.provider.executable().to_owned(),
                model: agent.spec.model.unwrap_or_default(),
                effort: agent.spec.reasoning_effort.unwrap_or_default(),
                channel,
                placement,
                state: Some(run.state),
                timed_out: data["timed_out"].as_bool().unwrap_or(false),
                result: run.result.text,
            })
        });
    let receipt = matches!(expected, "send" | "ask" | "reply")
        .then(|| serde_json::from_value::<crate::agent_execution::mailbox::Sent>(data.clone()).ok())
        .flatten()
        .map(|sent| {
            use crate::agent_execution::mailbox::Reach;
            use crate::chat::coordination::{Arrival, Receipt};
            Receipt {
                message: crate::agent_execution::mailbox::short(sent.message.id),
                recipient: sent.message.to.label(),
                arrival: match (sent.reach, sent.message.to.lineage) {
                    (_, None) => Arrival::User,
                    (Reach::Conversation, _) => Arrival::WhenIdle,
                    (Reach::Inbox, _) => Arrival::Inbox,
                    (Reach::Offline, _) => Arrival::NextSession,
                },
            }
        });
    let messages = data
        .get(
            if matches!(
                kind,
                Kind::Message(Outbound {
                    kind: OutboundKind::Send | OutboundKind::Ask | OutboundKind::Reply,
                    ..
                })
            ) {
                "replies"
            } else {
                "messages"
            },
        )
        .and_then(|messages| {
            serde_json::from_value::<Vec<crate::agent_execution::mailbox::Message>>(
                messages.clone(),
            )
            .ok()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|message| crate::chat::coordination::Received {
            id: crate::agent_execution::mailbox::short(message.id),
            from: message.from.label(),
            body: message.body,
        })
        .collect();
    Some(Outcome {
        run,
        receipt,
        messages,
        ..Outcome::default()
    })
}

fn read(kind: &Kind, output: &str) -> Outcome {
    match kind {
        // A prompt or a spawn starts a run, and prints it ahead of any
        // messages that arrived while the command waited on it.
        Kind::Message(Outbound {
            kind: OutboundKind::Prompt,
            ..
        })
        | Kind::Spawn(_) => Outcome {
            run: agents::read_run(output.trim_start()),
            messages: arrived(output),
            ..Outcome::default()
        },
        Kind::Message(_) => Outcome {
            receipt: output.lines().find_map(messages::read_sent_line),
            messages: messages::read_messages(output),
            ..Outcome::default()
        },
        Kind::Act(Act::Status { .. }) => Outcome {
            run: agents::read_run(output.trim_start()),
            ..Outcome::default()
        },
        Kind::Act(Act::Wait { .. }) => Outcome {
            run: agents::read_run(output.trim_start()),
            messages: arrived(output),
            ..Outcome::default()
        },
        Kind::Act(Act::Inbox { .. }) => Outcome {
            messages: messages::read_messages(output),
            ..Outcome::default()
        },
        Kind::Act(_) => Outcome::default(),
    }
}

/// The messages after a wait's run statuses, which a run's own result could
/// otherwise be mistaken for.
fn arrived(output: &str) -> Vec<crate::chat::coordination::Received> {
    output
        .split_once(agents::ARRIVED)
        .map_or_else(Vec::new, |(_, arrived)| messages::read_messages(arrived))
}

/// The leading subcommand words of `argv`, such as `terminal list`.
fn subcommand_path(argv: &[String]) -> String {
    let mut path = Vec::new();
    GRAMMAR.with_borrow(|grammar| {
        let mut command = grammar;
        for word in argv.iter().skip(1).filter(|word| !word.starts_with('-')) {
            let Some(next) = command.find_subcommand(word) else {
                break;
            };
            path.push(next.get_name().to_owned());
            command = next;
        }
    });
    path.join(" ")
}

#[cfg(test)]
mod tests {
    use crate::chat::coordination::{Act, Kind, OutboundKind, Text};

    fn parse(line: &str, stdin: Option<&str>) -> Option<Kind> {
        let argv: Vec<String> = line.split(' ').map(str::to_owned).collect();
        let stdin = stdin.map_or(Text::Unseen, |stdin| Text::Inline(stdin.into()));
        super::coordination(&argv, stdin)
    }

    #[test]
    fn messages_carry_their_recipient_body_and_whether_they_wait() {
        let Some(Kind::Message(sent)) = parse("cagents send 95fcd1b9 hello", None) else {
            panic!("a send is a message");
        };
        assert_eq!(
            (sent.kind, sent.to.as_str(), sent.body, sent.waits),
            (
                OutboundKind::Send,
                "95fcd1b9",
                Text::Inline("hello".into()),
                false
            )
        );
        let Some(Kind::Message(asked)) = parse("cagents ask parent - --timeout 30", Some("hi\n"))
        else {
            panic!("an ask is a message");
        };
        assert_eq!(
            (asked.kind, asked.body, asked.waits),
            (OutboundKind::Ask, Text::Inline("hi\n".into()), true)
        );
        let Some(Kind::Message(replied)) = parse("cagents reply 45d4 --file /tmp/a.md", None)
        else {
            panic!("a reply is a message");
        };
        assert_eq!(replied.body, Text::File("/tmp/a.md".into()));
        assert_eq!(
            parse("cagents send peer -", None).map(|kind| match kind {
                Kind::Message(message) => message.body,
                _ => Text::Unseen,
            }),
            Some(Text::Unseen)
        );
    }

    #[test]
    fn spawns_read_their_options_and_prompt() {
        let Some(Kind::Spawn(spawn)) = parse(
            "cagents agent spawn --name reviewer --provider codex --reasoning-effort low --wait Review",
            None,
        ) else {
            panic!("a spawn");
        };
        assert_eq!(spawn.agents[0].name, "reviewer");
        assert_eq!(spawn.agents[0].prompt, Text::Inline("Review".into()));
        assert_eq!(
            (spawn.provider.as_str(), spawn.effort.as_str(), spawn.waits),
            ("codex", "low", true)
        );
    }

    #[test]
    fn other_commands_are_named_and_invalid_ones_are_not_recognized() {
        assert_eq!(
            parse("cagents ls --all", None),
            Some(Kind::Act(Act::Sessions))
        );
        assert_eq!(
            parse("cagents agent wait a b --any", None),
            Some(Kind::Act(Act::Wait {
                agents: vec!["a".into(), "b".into()]
            }))
        );
        assert_eq!(
            parse("cagents --json terminal list", None),
            Some(Kind::Act(Act::Cli("terminal list".into())))
        );
        assert_eq!(
            parse("cagents linear issue CAG-12 --full", None),
            Some(Kind::Act(Act::Linear {
                verb: "Read ticket",
                subject: "CAG-12".into()
            }))
        );
        assert_eq!(parse("cagents send", None), None);
        // Standard input is read only for `-`, as the CLI reads it.
        assert_eq!(
            parse("cagents send peer", Some("hi\n")).map(|kind| match kind {
                Kind::Message(message) => message.body,
                _ => Text::Inline(String::new()),
            }),
            Some(Text::Unseen)
        );
        assert_eq!(parse("cagents agent console x", None), None);
        assert_eq!(parse("cagents --help", None), None);
    }

    fn json_run(command: &str, model: Option<&str>) -> serde_json::Value {
        use crate::agent_execution::{
            Agent, Origin, ResultSummary, Run, Spec, State, Surface, Target,
        };
        let agent = Agent {
            name: Some("reviewer".into()),
            id: crate::model::AgentId::new(),
            origin: Origin::User,
            spec: Spec {
                provider: crate::agent::Provider::Codex,
                account: None,
                target: Target {
                    repository: "/repo".into(),
                    directory: "/repo".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: model.map(str::to_owned),
                permission_mode: None,
                reasoning_effort: Some("low".into()),
            },
            placement: crate::agent_execution::Placement::Channel(crate::model::ChannelId::new()),
            session: None,
            console: None,
            created_ms: 0,
            closed: false,
        };
        let run = Run {
            id: crate::model::RunId::new(),
            agent: agent.id,
            state: State::Succeeded,
            revision: 1,
            accepted_ms: 0,
            started_ms: Some(1),
            finished_ms: Some(2),
            provider_key: None,
            result: ResultSummary {
                text: "Reviewed.".into(),
                ..Default::default()
            },
        };
        serde_json::json!({"ok": true, "command": command, "data": {"agent": agent, "run": run, "timed_out": false}})
    }

    #[test]
    fn json_run_resolves_launch_metadata_without_inventing_unknown_defaults() {
        let mut spawn = parse("cagents --json agent spawn Review", None).unwrap();
        let reply = json_run("agent.spawn", Some("gpt-6-luna"));
        let result = super::outcome(&spawn, &reply.to_string());
        let run = result.run.as_ref().unwrap();
        assert_eq!(run.model, "gpt-6-luna");
        assert_eq!(run.provider, "codex");
        assert_eq!(run.result, "Reviewed.");
        assert_eq!(run.state, Some(crate::agent_execution::State::Succeeded));
        super::enrich(&mut spawn, &result);
        let Kind::Spawn(spawn) = spawn else { panic!() };
        assert_eq!(spawn.channel, run.channel);
        assert!(spawn.channel.is_some());
        assert_eq!(
            (
                spawn.provider.as_str(),
                spawn.model.as_str(),
                spawn.effort.as_str()
            ),
            ("codex", "gpt-6-luna", "low")
        );

        let mut unknown = parse("cagents --json agent spawn Review", None).unwrap();
        let result = super::outcome(&unknown, &json_run("agent.spawn", None).to_string());
        super::enrich(&mut unknown, &result);
        let Kind::Spawn(unknown) = unknown else {
            panic!()
        };
        assert!(unknown.model.is_empty());
        let human = parse("cagents agent spawn Review", None).unwrap();
        let result = super::outcome(&human, "Accepted agent 1a2b3c4d run 5e6f7a8b");
        assert!(result.run.unwrap().channel.is_none());
    }

    #[test]
    fn accepted_worktree_scope_is_reported_without_an_invented_channel() {
        let mut parsed = parse("cagents --json agent spawn Review", None).unwrap();
        let mut reply = json_run("agent.spawn", None);
        let scope = crate::agent_execution::Placement::Worktree {
            repository: "/repo".into(),
            path: "/repo/worktree".into(),
        };
        reply["data"]["agent"]["placement"] = serde_json::to_value(&scope).unwrap();
        let outcome = super::outcome(&parsed, &reply.to_string());
        assert_eq!(outcome.run.as_ref().unwrap().placement, Some(scope.clone()));
        assert!(outcome.run.as_ref().unwrap().channel.is_none());
        super::enrich(&mut parsed, &outcome);
        let Kind::Spawn(spawn) = parsed else {
            unreachable!()
        };
        assert_eq!(spawn.placement, Some(scope));
        assert!(spawn.channel.is_none());
    }

    #[test]
    fn spawn_preserves_explicit_channel_and_model_before_a_reply_arrives() {
        let channel = crate::model::ChannelId::new();
        let Kind::Spawn(spawn) = parse(
            &format!("cagents agent spawn --channel {channel} --model gpt-6-luna Review"),
            None,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(spawn.channel, Some(channel));
        assert_eq!(spawn.model, "gpt-6-luna");
        assert!(spawn.provider.is_empty());
    }

    #[test]
    fn json_run_rejects_another_commands_reply_and_mismatched_agent_identity() {
        let spawn = parse("cagents --json agent spawn Review", None).unwrap();
        let mut reply = json_run("agent.status", Some("gpt-6-luna"));
        assert!(super::outcome(&spawn, &reply.to_string()).run.is_none());
        reply["command"] = "agent.spawn".into();
        reply["data"]["run"]["agent"] = serde_json::json!(crate::model::AgentId::new());
        assert!(super::outcome(&spawn, &reply.to_string()).run.is_none());
    }

    #[test]
    fn json_receipts_replies_and_failures_follow_the_cli_wire_objects() {
        use crate::agent_execution::mailbox::{Message, Party, Reach, State};
        let message = Message {
            id: crate::model::MessageId::new(),
            from: Party::user(),
            to: Party::user(),
            reply_to: None,
            body: "Can you review?".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms: 0,
        };
        let ask = parse("cagents --json ask peer hi", None).unwrap();
        let output = serde_json::json!({"ok": true, "command": "ask", "data": {"message": message, "reach": Reach::Inbox, "replies": [message]}});
        let result = super::outcome(&ask, &output.to_string());
        assert_eq!(
            result.receipt.unwrap().arrival,
            crate::chat::coordination::Arrival::User
        );
        assert_eq!(result.messages[0].body, "Can you review?");
        let inbox = parse("cagents --json inbox", None).unwrap();
        let output =
            serde_json::json!({"ok": true, "command": "inbox", "data": {"messages": [message]}});
        assert_eq!(
            super::outcome(&inbox, &output.to_string()).messages[0].body,
            "Can you review?"
        );
        let failed = serde_json::json!({"ok":false,"error":{"kind":"handle","message":"No such recipient","details":null}});
        assert_eq!(
            super::outcome(&ask, &failed.to_string()).error.as_deref(),
            Some("No such recipient")
        );
        let timed_out = serde_json::json!({"ok":false,"error":{"kind":"timeout","message":"Sent 45d4828a to session 95fcd1b9; it arrives as soon as that conversation is idle\nNo reply within 30 s."}});
        assert!(
            super::outcome(&ask, &timed_out.to_string())
                .receipt
                .is_some()
        );
    }

    #[test]
    fn a_spawn_reads_its_run_from_what_the_cli_printed() {
        let spawn = parse("cagents agent spawn hi", None).unwrap();
        let accepted = super::outcome(&spawn, "Accepted agent 1a2b3c4d run 5e6f7a8b\n");
        assert_eq!(accepted.run.unwrap().agent, "1a2b3c4d");
        let waited = super::outcome(
            &spawn,
            "agent 1a2b3c4d (codex) run 5e6f7a8b Succeeded\n\nAll green.\n\n\nMessages arrived while waiting:\n\nab12  from the user\n  hi",
        );
        let run = waited.run.unwrap();
        assert_eq!(run.provider, "codex");
        assert!(run.model.is_empty());
        assert!(run.channel.is_none());
        assert_eq!(run.state, Some(crate::agent_execution::State::Succeeded));
        assert_eq!(run.result, "All green.");
        assert_eq!(waited.messages.len(), 1);
        // A wait that messages ended before the run answered has no result.
        let interrupted = super::outcome(
            &spawn,
            "agent 1a2b3c4d (codex) run 5e6f7a8b Running\n\nMessages arrived while waiting:\n\nab12  from the user\n  hi\n\nAnswer one with: cagents reply <id> \"…\"",
        );
        assert_eq!(interrupted.run.unwrap().result, "");
        assert_eq!(interrupted.messages[0].body, "hi");
    }

    #[test]
    fn a_prompt_to_an_agent_reads_the_run_it_started() {
        let prompt = parse("cagents agent send 1a2b3c4d --wait Continue", None).unwrap();
        let outcome = super::outcome(
            &prompt,
            "agent 1a2b3c4d (claude) run 5e6f7a8b Succeeded\n\nDone.",
        );
        assert_eq!(outcome.run.unwrap().result, "Done.");
        assert_eq!(outcome.receipt, None);
    }

    #[test]
    fn an_ask_that_timed_out_still_reads_as_sent() {
        let ask = parse("cagents ask peer hi", None).unwrap();
        let outcome = super::outcome(
            &ask,
            "cagents: Sent 45d4828a to session 95fcd1b9; it arrives as soon as that conversation is idle\nNo reply within 30 s.",
        );
        assert_eq!(outcome.receipt.unwrap().message, "45d4828a");
    }
}
