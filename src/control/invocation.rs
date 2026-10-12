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
pub(crate) fn coordination(argv: &[String], stdin: Option<String>) -> Option<Kind> {
    let cli = GRAMMAR.with_borrow_mut(|grammar| {
        let matches = grammar.try_get_matches_from_mut(argv).ok()?;
        Cli::from_arg_matches(&matches).ok()
    })?;
    let text = |inline: Option<String>, file: Option<PathBuf>| match (inline, file) {
        (Some(inline), _) if inline != "-" => Text::Inline(inline),
        (None, Some(path)) => Text::File(path.display().to_string()),
        _ => stdin.clone().map_or(Text::Unseen, Text::Inline),
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
        TopCommand::Reply(reply) => message(OutboundKind::Reply, reply.id, reply.message, reply.wait),
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
    match kind {
        Kind::Message(_) => Outcome {
            receipt: output.lines().find_map(messages::read_sent_line),
            messages: messages::read_messages(output),
            run: None,
        },
        Kind::Spawn(_) => Outcome {
            run: agents::read_run(output.trim_start()),
            ..Outcome::default()
        },
        Kind::Act(Act::Inbox { .. } | Act::Wait { .. }) => Outcome {
            messages: messages::read_messages(output),
            ..Outcome::default()
        },
        Kind::Act(_) => Outcome::default(),
    }
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
        super::coordination(&argv, stdin.map(str::to_owned))
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
        assert_eq!(parse("cagents agent console x", None), None);
        assert_eq!(parse("cagents --help", None), None);
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
        assert_eq!(run.state, Some(crate::agent_execution::State::Succeeded));
        assert_eq!(run.result, "All green.");
    }
}
