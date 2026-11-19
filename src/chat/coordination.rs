//! What an agent did to work with other agents, read off the step it took.
//!
//! What arrives in a conversation from another agent already has a typed
//! item of its own, [`super::ItemContent::Delivery`]. What leaves it did not:
//! a message sent with `cagents send` was a shell command whose message was a
//! quoted argument, and a subagent started with Claude's `Agent` tool was a
//! tool call whose prompt was a JSON string. A reader asking "what did it tell
//! the other session?" had to read it out of an argv.
//!
//! Recognition happens here, at presentation, rather than in the adapters:
//! the command an agent typed is the same whichever provider ran it, and a
//! transcript recorded before this existed is read the same way as a live
//! one. The item itself is untouched, so the raw command and its output stay
//! one click away.

mod shell;

use super::{Item, ItemContent, ToolStatus};
use crate::agent::Provider;
use serde_json::Value;

/// One coordinating step, and the subagents of this conversation it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordination {
    pub kind: Kind,
    /// What the step's output says about how it went.
    pub outcome: Outcome,
    /// Subagents that the step started or addressed, by an ID a subagent of
    /// this conversation's snapshot answers to: the one it is listed under,
    /// or the one its provider addresses it by. A view offers only those it
    /// finds.
    pub subagents: Vec<String>,
    /// Whether the step is a `cagents` command, which addresses sessions,
    /// rather than a provider's own tool, which addresses its agents.
    pub cli: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Words for another agent: the part a reader wants to read.
    Message(Outbound),
    /// Agents started on a prompt.
    Spawn(Spawn),
    /// Anything else, which a step's one line describes.
    Act(Act),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub kind: OutboundKind,
    /// Who it is for, as the agent named them: a session ID or a prefix of
    /// one, `parent`, an agent's name, or the ID of the message answered.
    /// Empty where the provider names no one.
    pub to: String,
    pub body: Text,
    /// The sender's own one-line account of it, where it wrote one.
    pub summary: String,
    /// Whether the command holds the turn until an answer arrives.
    pub waits: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundKind {
    /// A message to another session.
    Send,
    /// A message whose sender waits for the answer.
    Ask,
    /// An answer to a message this session received.
    Reply,
    /// A follow-up prompt to an agent this one started.
    Prompt,
    /// A subagent's final report to the conversation that started it.
    Report,
}

/// A message or a prompt, as far as the step shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    Inline(String),
    /// Read from this file, whose contents the transcript never saw.
    File(String),
    /// Read from standard input the line does not show.
    Unseen,
}

impl Text {
    pub fn inline(&self) -> Option<&str> {
        match self {
            Self::Inline(text) => Some(text),
            Self::File(_) | Self::Unseen => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spawn {
    pub agents: Vec<Delegate>,
    /// A brief every agent shares, given beside their own tasks.
    pub brief: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub directory: String,
    /// Explicit membership or the channel returned by the CLI. Unknown for
    /// provider-native agents and commands whose output did not expose it.
    pub channel: Option<crate::model::ChannelId>,
    pub placement: Option<crate::agent_execution::Placement>,
    /// Whether the step holds the turn until the agents finish.
    pub waits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegate {
    /// The handle it was given, such as `reviewer`.
    pub name: String,
    /// Its type, such as `Explore`, `scout`, or a provider's role name.
    pub kind: String,
    /// The one line the starter wrote about the task.
    pub description: String,
    pub prompt: Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Sessions,
    Whoami,
    Inbox {
        waits: bool,
    },
    Agents,
    Status {
        agent: String,
    },
    Wait {
        agents: Vec<String>,
    },
    Requests {
        agent: String,
    },
    Answer {
        agent: String,
        request: String,
        action: String,
    },
    Interrupt {
        agent: String,
    },
    Close {
        agents: Vec<String>,
    },
    Resume {
        agents: Vec<String>,
    },
    /// Background work stopped by its task ID.
    Stop {
        task: String,
    },
    /// Background work's output read by its task ID.
    Output {
        task: String,
    },
    Linear {
        verb: &'static str,
        /// What it acted on: a ticket, a query, or a phrase for the ticket
        /// linked to this checkout. Empty when the verb says it all.
        subject: String,
    },
    /// Any other `cagents` command, by its subcommand path.
    Cli(String),
}

/// What a `cagents` command reported about the step, read from its output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub receipt: Option<Receipt>,
    /// What a provider's own message tool said became of the message.
    pub delivery: Option<Delivery>,
    /// Messages the command printed: the answers an `ask` waited for, or
    /// what an inbox held.
    pub messages: Vec<Received>,
    pub run: Option<Run>,
    /// What the CLI said went wrong, which a pipeline such as `| tail -1`
    /// can print while the step itself reports success.
    pub error: Option<String>,
}

/// The CLI's confirmation that a message was accepted for delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The message's short ID, which an answer to it refers to.
    pub message: String,
    /// The recipient as the CLI names it: `session 95fcd1b9 "Title"`.
    pub recipient: String,
    pub arrival: Arrival,
}

pub use crate::control::Arrival;

/// What became of a message a provider's own tool sent to one of its agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Accepted for the agent to read at its next step.
    Queued,
    /// The agent had stopped, and the message started it again.
    Resumed,
    /// Left in another session's inbox for it to read.
    Inbox,
    /// Accepted, in words this reading does not know.
    Sent,
}

/// A message as the CLI prints one it received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub id: String,
    /// The sender as the CLI names it.
    pub from: String,
    pub body: String,
}

/// A started agent's run, as far as the command saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    /// The agent's short ID.
    pub agent: String,
    /// Resolved launch metadata, where the CLI's reply exposed it. Empty
    /// strings and an absent channel mean unknown, never inherited guesses.
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub channel: Option<crate::model::ChannelId>,
    pub placement: Option<crate::agent_execution::Placement>,
    /// The channel it went to, where the CLI said that is not the starting
    /// conversation's own view.
    pub placed_in: Option<crate::control::PlacedIn>,
    /// Where the command waited on it, how the run stood when it stopped.
    pub state: Option<crate::agent_execution::State>,
    pub timed_out: bool,
    /// The end of what the agent answered, where the command waited for it.
    pub result: String,
}

/// A bounded candidate check for callers which schedule recognition off-thread.
/// Short ordinary commands can be excluded without parsing shell syntax. Long
/// commands are always candidates; arbitrary quoting/JSON is read by the worker.
pub fn may_recognize(item: &Item, provider: Provider) -> bool {
    match &item.content {
        ItemContent::Command { command, .. } => command.len() > 2048 || names_program(command),
        ItemContent::Tool { name, .. } => match provider {
            Provider::Claude => matches!(
                name.as_str(),
                "Agent"
                    | "Task"
                    | "SendMessage"
                    | "SubagentHandback"
                    | "TaskStop"
                    | "KillShell"
                    | "TaskOutput"
            ),
            Provider::Codex => matches!(
                name.as_str(),
                "spawnAgent"
                    | "spawn_agent"
                    | "sendInput"
                    | "send_input"
                    | "wait"
                    | "closeAgent"
                    | "close_agent"
                    | "resumeAgent"
                    | "resume_agent"
                    | "sendMessage"
                    | "followupTask"
                    | "interruptAgent"
                    | "listAgents"
            ),
            Provider::Omp => name == "task",
            // Their coordination tools aren't read yet.
            Provider::Antigravity | Provider::Muse => false,
        },
        _ => false,
    }
}

/// What `item` did to coordinate, when it did anything of the kind.
///
/// `provider` is the conversation's, which is what gives a tool's name its
/// meaning: a tool called `task` is OMP's subagent launcher and nobody
/// else's.
pub fn recognize(item: &Item, provider: Provider) -> Option<Coordination> {
    if !may_recognize(item, provider) {
        return None;
    }
    match &item.content {
        ItemContent::Command {
            command, output, ..
        } => cli(command, output),
        ItemContent::Tool {
            name,
            input,
            output,
            ..
        } => native(provider, &item.id, name, input, output),
        _ => None,
    }
}

impl Coordination {
    /// Whether the step is drawn as a card of its own, for the words it
    /// carries, rather than as one line among the others.
    pub fn is_card(&self) -> bool {
        matches!(self.kind, Kind::Message(_) | Kind::Spawn(_))
    }

    /// Whether the step's output is the provider's record of the agents it
    /// addressed, such as Codex's map of thread IDs to states, rather than
    /// text for a reader. The step's line and the agents' own rows already
    /// say what it did, and the record stays in the raw call.
    pub fn output_is_agent_states(&self) -> bool {
        !self.cli
            && matches!(
                self.kind,
                Kind::Act(
                    Act::Wait { .. }
                        | Act::Close { .. }
                        | Act::Resume { .. }
                        | Act::Interrupt { .. }
                        | Act::Agents
                )
            )
    }

    /// How the step went, given how its item `reported` it went: a message
    /// or a launch whose exit status a pipeline hid still failed if the CLI
    /// said so and confirmed nothing.
    pub fn status(&self, reported: ToolStatus) -> ToolStatus {
        let outcome = &self.outcome;
        let confirmed = outcome.receipt.is_some()
            || outcome.delivery.is_some()
            || outcome.run.is_some()
            || !outcome.messages.is_empty();
        match (&self.kind, reported) {
            (Kind::Message(_) | Kind::Spawn(_), ToolStatus::Succeeded)
                if outcome.error.is_some() && !confirmed =>
            {
                ToolStatus::Failed
            }
            (_, reported) => reported,
        }
    }

    /// Whether the step reached another agent, rather than the application,
    /// a ticket, or a background shell.
    pub fn involves_agents(&self) -> bool {
        !matches!(
            self.kind,
            Kind::Act(
                Act::Sessions
                    | Act::Whoami
                    | Act::Stop { .. }
                    | Act::Output { .. }
                    | Act::Linear { .. }
                    | Act::Cli(_)
            )
        )
    }
}

/// The program name that marks a line worth lexing at all, so the common
/// command costs one substring search.
const PROGRAM: &str = "cagents";

/// Whether `line` has `cagents` as a whole word, or the last part of a path,
/// as a command would. A checkout called `cagents` puts the name in most
/// paths, which only ever continue past it.
fn names_program(line: &str) -> bool {
    line.match_indices(PROGRAM).any(|(at, _)| {
        line[at + PROGRAM.len()..]
            .chars()
            .next()
            .is_none_or(|next| next.is_whitespace() || "'\";)|&".contains(next))
    })
}

/// A shell line that is one `cagents` invocation, read as what it did.
///
/// The whole line has to be that invocation: a step is labelled by what it
/// is, not by a command it contains. Around it the line may only set up where
/// and when it runs (`cd`, `export`, `set`, `sleep`), feed it a message the
/// line itself holds (`cat <<EOF |`, `echo … |`), trim what it printed
/// (`| tail -1`, `| jq …`), or say how it went (`|| echo failed`). A line that
/// also lists a directory, searches files or runs a second invocation is a
/// command of its own, and its row says so.
fn cli(line: &str, output: &str) -> Option<Coordination> {
    if !names_program(line) {
        return None;
    }
    let commands = shell::commands(line)?;
    let index = commands
        .iter()
        .position(|simple| invocation(&simple.argv).is_some())?;
    let simple = &commands[index];
    // One that may not have run reads as nothing it did.
    if simple.guarded {
        return None;
    }
    let argv = invocation(&simple.argv)?;
    // The invocation's own pipeline: what feeds it, and what reads it.
    let feeds = simple.stdin == shell::Stdin::Pipe;
    let first = if feeds { index.checked_sub(1)? } else { index };
    let last = index
        + commands[index + 1..]
            .iter()
            .take_while(|after| after.stdin == shell::Stdin::Pipe)
            .count();
    // What is piped in reaches the CLI only where it reads standard input,
    // as a message, a prompt, a ticket body or a definition given as `-`;
    // piped into anything else, it is a command of its own.
    if feeds && !crate::control::invocation::reads_stdin(argv) {
        return None;
    }
    let incidental = commands.iter().enumerate().all(|(at, other)| match at {
        at if at == index => true,
        at if at < first => sets_up(other),
        at if at < index => piped_text(other).is_some() || piped_file(other).is_some(),
        at if at <= last => trims(other),
        _ => reports(other),
    });
    if !incidental {
        return None;
    }
    let stdin = match &simple.stdin {
        shell::Stdin::Text(text) => Text::Inline(text.clone()),
        shell::Stdin::File(path) => Text::File(path.clone()),
        shell::Stdin::Pipe => match commands.get(first) {
            Some(before) => match (piped_text(before), piped_file(before)) {
                (Some(text), _) => Text::Inline(text),
                (None, Some(file)) => Text::File(file.to_owned()),
                (None, None) => Text::Unseen,
            },
            None => Text::Unseen,
        },
        shell::Stdin::Unknown => Text::Unseen,
    };
    let mut kind = crate::control::invocation::coordination(argv, stdin)?;
    let outcome = crate::control::invocation::outcome(&kind, output);
    crate::control::invocation::enrich(&mut kind, &outcome);
    Some(Coordination {
        outcome,
        kind,
        subagents: Vec::new(),
        cli: true,
    })
}

/// The program `simple` runs, by its name rather than its path.
fn program(simple: &shell::Simple) -> &str {
    simple
        .argv
        .first()
        .and_then(|program| program.rsplit('/').next())
        .unwrap_or_default()
}

/// Whether `simple`, run before an invocation, only sets up where and when
/// it runs. `cd repo || exit 1` gives up when it cannot.
fn sets_up(simple: &shell::Simple) -> bool {
    match program(simple) {
        "cd" | "pushd" | "export" | "set" | "unset" | "sleep" | "true" | ":" => true,
        "exit" => simple.guarded,
        _ => false,
    }
}

/// Whether `simple`, reading an invocation's output through a pipe, only
/// trims, filters or formats what it printed. One that names a file to read
/// as well, as `head -50 build.log` or `jq --slurpfile seen seen.json` do,
/// or searches a directory, as `grep -r` does, reads something else.
fn trims(simple: &shell::Simple) -> bool {
    let Some(filter) = Filter::of(program(simple)) else {
        return false;
    };
    // `tee` writes its files, so they are no input.
    if filter.program == usize::MAX {
        return true;
    }
    let mut programs = filter.program;
    let mut operands = 0;
    let mut words = simple.argv[1..].iter();
    while let Some(word) = words.next() {
        if word == "--" {
            operands += words.len();
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            if filter.reads_more.contains(&name) {
                return false;
            }
            if filter.supplies_program.contains(&name) {
                programs = 0;
            }
            let values = filter
                .long
                .iter()
                .find(|(option, _)| *option == name)
                .map_or(0, |(_, values)| *values);
            for _ in 0..values.saturating_sub(usize::from(attached)) {
                words.next();
            }
            continue;
        }
        if let Some(cluster) = word.strip_prefix('-').filter(|cluster| !cluster.is_empty()) {
            // `-ne PATTERN`, `-n5`, `-eerror`: a short option taking a value
            // takes the rest of its word, or the next word.
            for (at, option) in cluster.char_indices() {
                if filter
                    .reads_more
                    .contains(&&cluster[at..at + option.len_utf8()])
                {
                    return false;
                }
                if !filter.short.contains(option) {
                    continue;
                }
                if filter.supplies_program.contains(&&cluster[at..at + 1]) {
                    programs = 0;
                }
                if at + option.len_utf8() == cluster.len() {
                    words.next();
                }
                break;
            }
            continue;
        }
        operands += 1;
    }
    operands <= programs
}

/// How a program that filters its input reads its command line.
struct Filter {
    /// How many operands are its program (a pattern, a script, a filter)
    /// rather than files to read; `usize::MAX` for one whose operands are
    /// files it writes.
    program: usize,
    /// Short options that take a value.
    short: &'static str,
    /// Long options that take values, and how many.
    long: &'static [(&'static str, usize)],
    /// Options, short or long, that give the program, so no operand does.
    supplies_program: &'static [&'static str],
    /// Options, short or long, that read input besides the piped output: a
    /// file of data, or a directory searched in place of standard input.
    reads_more: &'static [&'static str],
}

impl Filter {
    fn of(program: &str) -> Option<Self> {
        let filter = |program, short, long, supplies_program| Self {
            program,
            short,
            long,
            supplies_program,
            reads_more: &[],
        };
        Some(match program {
            "tee" => filter(usize::MAX, "", &[], &[]),
            "head" | "tail" => filter(0, "nc", &[("lines", 1), ("bytes", 1)], &[]),
            "wc" | "cat" | "less" | "more" => filter(0, "", &[], &[]),
            "nl" => filter(
                0,
                "bdfhilnsvw",
                &[
                    ("body-numbering", 1),
                    ("section-delimiter", 1),
                    ("footer-numbering", 1),
                    ("header-numbering", 1),
                    ("line-increment", 1),
                    ("join-blank-lines", 1),
                    ("number-format", 1),
                    ("number-separator", 1),
                    ("starting-line-number", 1),
                    ("number-width", 1),
                ],
                &[],
            ),
            "sort" => filter(
                0,
                "ktoST",
                &[
                    ("key", 1),
                    ("field-separator", 1),
                    ("output", 1),
                    ("buffer-size", 1),
                ],
                &[],
            ),
            "uniq" => filter(
                0,
                "fsw",
                &[("skip-fields", 1), ("skip-chars", 1), ("check-chars", 1)],
                &[],
            ),
            "fold" => filter(0, "w", &[("width", 1)], &[]),
            "column" => filter(
                0,
                "scoNRWTEHOnl",
                &[
                    ("separator", 1),
                    ("output-separator", 1),
                    ("output-width", 1),
                    ("table-columns", 1),
                    ("table-right", 1),
                    ("table-wrap", 1),
                    ("table-truncate", 1),
                    ("table-noextreme", 1),
                    ("table-hide", 1),
                    ("table-order", 1),
                    ("table-name", 1),
                    ("table-columns-limit", 1),
                ],
                &[],
            ),
            "cut" => filter(
                0,
                "dfcb",
                &[
                    ("delimiter", 1),
                    ("fields", 1),
                    ("characters", 1),
                    ("bytes", 1),
                ],
                &[],
            ),
            "tr" => filter(2, "", &[], &[]),
            // With no file, `grep -r` searches the working directory, not
            // what it is piped.
            "grep" | "egrep" | "fgrep" => Self {
                reads_more: &["r", "R", "recursive", "dereference-recursive"],
                ..filter(
                    1,
                    "efmABCd",
                    &[
                        ("regexp", 1),
                        ("file", 1),
                        ("max-count", 1),
                        ("after-context", 1),
                        ("before-context", 1),
                        ("context", 1),
                        ("label", 1),
                    ],
                    &["e", "f", "regexp", "file"],
                )
            },
            // Piped input with no path, `rg` searches that input.
            "rg" => filter(
                1,
                "efmABCgtTrM",
                &[
                    ("regexp", 1),
                    ("file", 1),
                    ("max-count", 1),
                    ("after-context", 1),
                    ("before-context", 1),
                    ("context", 1),
                    ("glob", 1),
                    ("iglob", 1),
                    ("type", 1),
                    ("type-not", 1),
                    ("replace", 1),
                    ("max-columns", 1),
                ],
                &["e", "f", "regexp", "file"],
            ),
            "sed" => filter(
                1,
                "ef",
                &[("expression", 1), ("file", 1)],
                &["e", "f", "expression", "file"],
            ),
            "awk" => filter(
                1,
                "Fvf",
                &[("field-separator", 1), ("assign", 1), ("file", 1)],
                &["f", "file"],
            ),
            // `--slurpfile` and `--rawfile` bind a file's contents.
            "jq" => Self {
                reads_more: &["slurpfile", "rawfile"],
                ..filter(
                    1,
                    "fL",
                    &[("arg", 2), ("argjson", 2), ("indent", 1), ("from-file", 1)],
                    &["f", "from-file"],
                )
            },
            _ => return None,
        })
    }
}

/// Whether `simple`, piped into an invocation, is `cat` of one file: the
/// message is that file, as `< file` would give it.
fn piped_file(simple: &shell::Simple) -> Option<&str> {
    match simple.argv.as_slice() {
        [_, file] if program(simple) == "cat" && !file.starts_with('-') => Some(file),
        _ => None,
    }
}

/// Whether `simple`, run after an invocation's pipeline, only reports how it
/// went, as `|| echo failed` or `; echo "exit $?"` do.
fn reports(simple: &shell::Simple) -> bool {
    matches!(program(simple), "echo" | "printf" | "true" | ":")
}

/// `argv` from the `cagents` word on, past the wrappers that only change how
/// it runs: `env`, `timeout`, `command`, `exec`, `nohup`, `time`.
fn invocation(argv: &[String]) -> Option<&[String]> {
    let mut rest = argv;
    loop {
        let (program, arguments) = rest.split_first()?;
        match program.rsplit('/').next()? {
            PROGRAM => return Some(rest),
            "command" | "exec" | "nohup" | "time" => rest = arguments,
            "env" => {
                let skip = arguments
                    .iter()
                    .take_while(|word| word.starts_with('-') || word.contains('='))
                    .count();
                rest = &arguments[skip..];
            }
            "timeout" => {
                // Options, then the duration.
                let options = arguments
                    .iter()
                    .take_while(|word| word.starts_with('-'))
                    .count();
                rest = arguments.get(options + 1..)?;
            }
            _ => return None,
        }
    }
}

/// What the command before a pipe writes, where the line itself holds it:
/// `cat` of a heredoc, or an `echo` or `printf '%s\n'` of literal words.
fn piped_text(before: &shell::Simple) -> Option<String> {
    let (program, arguments) = before.argv.split_first()?;
    match (program.as_str(), arguments) {
        ("cat", []) => match &before.stdin {
            shell::Stdin::Text(text) => Some(text.clone()),
            _ => None,
        },
        ("echo", words) if words.first().is_none_or(|word| !word.starts_with('-')) => {
            Some(format!("{}\n", words.join(" ")))
        }
        // A format reused for each word: `%s\n` writes a line per word.
        ("printf", [format, words @ ..]) if format == "%s\\n" => {
            Some(words.iter().map(|word| format!("{word}\n")).collect())
        }
        ("printf", [format, words @ ..]) if format == "%s" => Some(words.concat()),
        _ => None,
    }
}

/// A provider's own coordination tools.
fn native(
    provider: Provider,
    id: &str,
    name: &str,
    input: &str,
    output: &str,
) -> Option<Coordination> {
    match provider {
        Provider::Claude => claude(id, name, input, output),
        Provider::Codex => codex(name, input, output),
        Provider::Omp => omp(name, input),
        Provider::Antigravity | Provider::Muse => None,
    }
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn claude(id: &str, name: &str, input: &str, output: &str) -> Option<Coordination> {
    let input: Value = serde_json::from_str(input).ok()?;
    let mut subagents = Vec::new();
    let mut outcome = Outcome::default();
    let kind =
        match name {
            // `Task` is the tool's name before Claude Code renamed it.
            "Agent" | "Task" => {
                // The tool call's ID is the subagent's: Claude reports its
                // progress under the call that started it.
                subagents.push(id.to_owned());
                // Claude Code may background an agent it was not asked to, and
                // says so in place of a report.
                let waits = input["run_in_background"].as_bool() != Some(true)
                    && !output.trim_start().starts_with("Async agent launched");
                // A foreground agent's result is its report. A background one's
                // is the harness acknowledging the launch, and its report comes
                // later as a delivery of its own.
                if waits && let Some(result) = claude_report(output) {
                    outcome.run = Some(Run {
                        agent: String::new(),
                        state: None,
                        timed_out: false,
                        result,
                        ..Run::default()
                    });
                }
                Kind::Spawn(Spawn {
                    agents: vec![Delegate {
                        name: field(&input, "name").into(),
                        kind: field(&input, "subagent_type").into(),
                        description: field(&input, "description").into(),
                        prompt: Text::Inline(field(&input, "prompt").into()),
                    }],
                    model: field(&input, "model").into(),
                    waits,
                    ..Spawn::default()
                })
            }
            "SendMessage" => {
                let to = field(&input, "to");
                let result = claude_sent(output);
                outcome.delivery = result.delivery;
                outcome.error = result.error;
                // The agent the result pinned is the one it reached, whatever
                // name the call gave it. Every teammate (`*`) and another
                // session (`uds:…`) are none of this conversation's agents.
                subagents.extend(result.agent.or_else(|| {
                    (!to.is_empty() && to != "*" && !to.contains(':')).then(|| to.into())
                }));
                Kind::Message(Outbound {
                    kind: OutboundKind::Send,
                    to: to.into(),
                    body: Text::Inline(match &input["message"] {
                        Value::String(text) => text.clone(),
                        Value::Null => String::new(),
                        // A structured protocol message, such as a shutdown request.
                        other => other.to_string(),
                    }),
                    summary: field(&input, "summary").into(),
                    waits: false,
                })
            }
            "SubagentHandback" => Kind::Message(Outbound {
                kind: OutboundKind::Report,
                to: String::new(),
                body: Text::Inline(field(&input, "message").into()),
                summary: String::new(),
                waits: false,
            }),
            // A background agent's task is its agent ID, so the task may be
            // one of this conversation's agents as well as a shell.
            "TaskStop" | "KillShell" => {
                let task: String = ["task_id", "shell_id"]
                    .into_iter()
                    .map(|key| field(&input, key))
                    .find(|task| !task.is_empty())
                    .unwrap_or_default()
                    .into();
                subagents.extend((!task.is_empty()).then(|| task.clone()));
                Kind::Act(Act::Stop { task })
            }
            "TaskOutput" => {
                let task: String = field(&input, "task_id").into();
                subagents.extend((!task.is_empty()).then(|| task.clone()));
                Kind::Act(Act::Output { task })
            }
            _ => return None,
        };
    Some(Coordination {
        kind,
        outcome,
        subagents,
        cli: false,
    })
}

/// What Claude Code's `SendMessage` result says of a message.
#[derive(Default)]
struct Sent {
    delivery: Option<Delivery>,
    error: Option<String>,
    /// The agent ID of the agent it reached.
    agent: Option<String>,
}

/// `SendMessage` answers with JSON: whether it went, a sentence on how, and
/// the agent it went to, as `{"success":true,"message":"Message queued for
/// delivery to reviewer at its next tool round.","pin":{"id":"a03…",
/// "name":"reviewer"}}`. Nothing is read from a result in another shape.
fn claude_sent(output: &str) -> Sent {
    let Ok(result) = serde_json::from_str::<Value>(output.trim()) else {
        return Sent::default();
    };
    let said = field(&result, "message");
    let agent = ["resumedAgentId", "revivedAgentId"]
        .into_iter()
        .map(|key| field(&result, key))
        .chain([field(&result["pin"], "id")])
        .find(|agent| !agent.is_empty())
        .map(str::to_owned);
    match result["success"].as_bool() {
        Some(true) => Sent {
            delivery: Some(
                if result["resumedAgentId"].is_string()
                    || result["revivedAgentId"].is_string()
                    || said.starts_with("Resuming agent")
                {
                    Delivery::Resumed
                } else if said.contains("inbox") {
                    Delivery::Inbox
                } else if said.contains("queued") {
                    Delivery::Queued
                } else {
                    Delivery::Sent
                },
            ),
            error: None,
            agent,
        },
        Some(false) => Sent {
            delivery: None,
            // Its first line; the rest tells the model what to try next.
            error: Some(
                said.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("Claude did not send it")
                    .to_owned(),
            ),
            agent,
        },
        None => Sent::default(),
    }
}

/// A foreground agent's report, without the usage Claude Code appends to it.
/// `None` where the agent handed its report back as a message instead, which
/// the transcript shows as a delivery of its own.
fn claude_report(output: &str) -> Option<String> {
    let report = match output.rfind("agentId: ") {
        Some(0) => "",
        Some(at) if output[..at].ends_with('\n') => &output[..at],
        _ => output,
    }
    .trim();
    let handed_back = report.starts_with("This agent's report was delivered to you as a message");
    (!report.is_empty() && !handed_back).then(|| report.to_owned())
}

/// Codex reports a collaboration call with its prompt as the input and, as
/// the output, the state of every agent it addressed keyed by thread ID,
/// which is also the ID its subagents are listed under.
fn codex(name: &str, input: &str, output: &str) -> Option<Coordination> {
    let receivers: Vec<String> = serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|states| states.as_object().map(|map| map.keys().cloned().collect()))
        .unwrap_or_default();
    let kind = match name {
        "spawnAgent" | "spawn_agent" => Kind::Spawn(Spawn {
            agents: vec![Delegate {
                name: String::new(),
                kind: String::new(),
                description: String::new(),
                prompt: Text::Inline(input.into()),
            }],
            ..Spawn::default()
        }),
        "sendInput" | "send_input" => Kind::Message(Outbound {
            kind: OutboundKind::Prompt,
            to: receivers.join(", "),
            body: Text::Inline(input.into()),
            summary: String::new(),
            waits: false,
        }),
        "wait" => Kind::Act(Act::Wait {
            agents: receivers.clone(),
        }),
        "closeAgent" | "close_agent" => Kind::Act(Act::Close {
            agents: receivers.clone(),
        }),
        "resumeAgent" | "resume_agent" => Kind::Act(Act::Resume {
            agents: receivers.clone(),
        }),
        // A message for an agent to read, where `sendInput` hands it its next
        // turn's prompt.
        "sendMessage" => Kind::Message(Outbound {
            kind: OutboundKind::Send,
            to: receivers.join(", "),
            body: Text::Inline(input.into()),
            summary: String::new(),
            waits: false,
        }),
        "followupTask" => Kind::Message(Outbound {
            kind: OutboundKind::Prompt,
            to: receivers.join(", "),
            body: Text::Inline(input.into()),
            summary: String::new(),
            waits: false,
        }),
        "interruptAgent" => Kind::Act(Act::Interrupt {
            agent: receivers.join(", "),
        }),
        "listAgents" => Kind::Act(Act::Agents),
        _ => return None,
    };
    Some(Coordination {
        kind,
        outcome: Outcome::default(),
        subagents: receivers,
        cli: false,
    })
}

/// OMP's `task` starts one agent per entry of `tasks`, each under its own
/// name, beside a `context` they all share.
fn omp(name: &str, input: &str) -> Option<Coordination> {
    if name != "task" {
        return None;
    }
    let input: Value = serde_json::from_str(input).ok()?;
    let delegate = |task: &Value| Delegate {
        name: field(task, "name").into(),
        kind: field(task, "agent").into(),
        description: field(task, "description").into(),
        prompt: Text::Inline(field(task, "task").into()),
    };
    let mut agents: Vec<Delegate> = match input["tasks"].as_array() {
        Some(tasks) => tasks.iter().map(delegate).collect(),
        None if input["task"].is_string() => vec![delegate(&input)],
        None => return None,
    };
    // A job is listed under the name it was given. That name is its ID,
    // which its tab does not go by: it goes by its description, else its
    // type, and so does the step that started it.
    let subagents = agents
        .iter_mut()
        .filter(|agent| !agent.name.is_empty())
        .map(|agent| std::mem::take(&mut agent.name))
        .collect();
    let mut spawn = Spawn {
        agents,
        brief: field(&input, "context").into(),
        model: field(&input, "model").into(),
        ..Spawn::default()
    };
    // `i` is the intent OMP's tool calls carry, its summary of the call.
    if let [only] = spawn.agents.as_mut_slice()
        && only.description.is_empty()
    {
        only.description = field(&input, "i").into();
    }
    Some(Coordination {
        kind: Kind::Spawn(spawn),
        outcome: Outcome::default(),
        subagents,
        cli: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_cli_failures_cannot_hide_behind_a_successful_shell_pipeline() {
        let failure = serde_json::json!({"ok": false, "error": {"kind": "handle", "message": "No such recipient"}});
        let failed = cli(
            "cagents --json send peer hi 2>&1 | tail -1",
            &failure.to_string(),
        )
        .unwrap();
        assert_eq!(failed.status(ToolStatus::Succeeded), ToolStatus::Failed);
        assert_eq!(failed.outcome.error.as_deref(), Some("No such recipient"));
        let timeout = serde_json::json!({"ok": false, "error": {"kind": "timeout", "message": "Sent 45d4828a to session 95fcd1b9; it arrives as soon as that conversation is idle\nNo reply within 30 s."}});
        let sent = cli(
            "cagents --json ask peer hi 2>&1 | tail -1",
            &timeout.to_string(),
        )
        .unwrap();
        assert!(sent.outcome.receipt.is_some());
        // Waiting failed; delivery itself was confirmed, just as in human output.
        assert_eq!(sent.status(ToolStatus::Succeeded), ToolStatus::Succeeded);
        assert_eq!(sent.status(ToolStatus::Failed), ToolStatus::Failed);
    }

    #[test]
    fn a_sent_message_reads_its_recipient_body_and_receipt() {
        let sent = cli(
            r#"cagents send 95fcd1b9 "From the user: please stop sending" 2>&1 | tail -1"#,
            "Sent 45d4828a to session 95fcd1b9; it arrives as soon as that conversation is idle\n",
        )
        .expect("a send");
        assert!(sent.is_card());
        assert_eq!(sent.status(ToolStatus::Succeeded), ToolStatus::Succeeded);
        // `| tail -1` hides the exit status of a send the daemon refused.
        let refused = cli(
            "cagents send e493822d hi 2>&1 | tail -1",
            "cagents: InvalidRequest: That is you; a message must go to another session",
        )
        .unwrap();
        assert_eq!(refused.status(ToolStatus::Succeeded), ToolStatus::Failed);
        assert_eq!(
            refused.outcome.error.as_deref(),
            Some("InvalidRequest: That is you; a message must go to another session")
        );
        let Kind::Message(message) = &sent.kind else {
            panic!("a send is a message");
        };
        assert_eq!(
            (message.kind, message.to.as_str(), &message.body),
            (
                OutboundKind::Send,
                "95fcd1b9",
                &Text::Inline("From the user: please stop sending".into())
            )
        );
        let receipt = sent.outcome.receipt.expect("the CLI printed a receipt");
        assert_eq!(
            (receipt.message.as_str(), receipt.arrival),
            ("45d4828a", Arrival::WhenIdle)
        );
        // An ask confirms itself with its answer, and the CLI's notices
        // about its own diagnostics are no failure of the step.
        let notice = format!("cagents: {}", crate::diagnostics::FAILURE_NOTICE);
        let answered = cli(
            "cagents ask peer 'ok?' 2>&1",
            &format!("{notice}\nab12  from session 95fcd1b9\n  yes\n"),
        )
        .unwrap();
        assert_eq!(
            answered.status(ToolStatus::Succeeded),
            ToolStatus::Succeeded
        );
        let noted = cli("cagents send peer hi 2>&1 | tail -1", &notice).unwrap();
        assert_eq!(noted.outcome.error, None);
        // Only a message or a launch is read for a failure its exit status
        // hid: other commands print what they relay.
        let relayed = cli("cagents sessions | tail -5", "cagents: earlier failure\n").unwrap();
        assert_eq!(relayed.status(ToolStatus::Succeeded), ToolStatus::Succeeded);
    }

    #[test]
    fn a_message_piped_or_redirected_in_is_read_from_the_line() {
        let heredoc = cli("cagents ask parent - <<'EOF'\nShip it?\nEOF", "").unwrap();
        let echoed = cli("echo Ship it? | cagents send parent -", "").unwrap();
        for coordination in [heredoc, echoed] {
            let Kind::Message(message) = coordination.kind else {
                panic!("a message");
            };
            assert_eq!(message.body, Text::Inline("Ship it?\n".into()));
        }
        let wrapped = cli("timeout 60 env FOO=1 cagents inbox --wait", "").unwrap();
        assert_eq!(wrapped.kind, Kind::Act(Act::Inbox { waits: true }));
    }

    #[test]
    fn lines_that_are_not_one_cagents_invocation_are_left_alone() {
        assert!(cli("cargo test", "").is_none());
        assert!(cli("grep cagents src/main.rs", "").is_none());
        assert!(cli("cagents send a hi && cagents send b hi", "").is_none());
        assert!(cli("echo $(cagents whoami)", "").is_none());
        // It may not run, or run once for each pass of a loop.
        assert!(cli("true || cagents send x hi", "").is_none());
        assert!(cli("for s in a b; do\n  cagents send \"$s\" hi\ndone", "").is_none());
        assert!(cli("cagents send x hi || echo failed", "").is_some());
        // A line that only contains an invocation is not that invocation.
        assert!(cli("git diff | cagents send reviewer -", "").is_none());
        // A checkout named for the program puts it in paths, not commands.
        assert!(!names_program(
            "cd ~/src/cagents/.worktrees/x && cargo test"
        ));
        assert!(names_program(
            "~/src/cagents/target/debug/cagents send x hi"
        ));
        assert!(names_program("echo hi|cagents"));
    }

    /// The line from the issue that labelled a look around the
    /// configuration directory "Listed live sessions", and the compound
    /// commands agents run alongside an invocation. A step is what the
    /// whole line is.
    #[test]
    fn a_compound_command_is_not_the_invocation_it_contains() {
        let output = r#"[{"id":"95fcd1b9","title":"Relocate the repo"}]"#;
        for line in [
            "ls ~/.config/cagents/ 2>/dev/null; find ~/.config/cagents -maxdepth 2 -name '*.toml' 2>/dev/null; timeout 20 cagents --device fw16 --no-start sessions --json 2>&1 | head -50",
            "cat ~/.config/cagents/devices.toml && cagents --device fw16 sessions",
            "cagents sessions --json | jq length; git status --short",
            "cargo build -q && cagents send parent 'build passed'",
            "cagents whoami && cagents inbox",
            "gh pr view 3110 --json title | cagents send reviewer -",
            "/bin/bash -lc 'ls -la && cagents sessions'",
            "for host in fw16 local; do cagents --device \"$host\" sessions; done",
            // A filter that also reads a file of its own.
            "cagents sessions | head -50 build.log",
            "cagents inbox | grep -n error notes.md",
            "cagents sessions | grep -eerror build.log",
            "cagents sessions | grep --regexp=error build.log",
            "cagents sessions --json | jq --slurpfile seen seen.json '.'",
            "cagents sessions --json | jq --rawfile notes notes.md '.'",
            // With no file, a recursive grep searches the working directory.
            "cagents sessions | grep -r Relocate",
            "cagents inbox | grep -rn error",
            "cagents sessions | grep --recursive Relocate",
            // What is piped into a command that does not read it: `--file -`
            // opens a file called `-`.
            "cat review.md | cagents sessions",
            "echo hi | cagents send reviewer --file -",
        ] {
            assert!(cli(line, output).is_none(), "{line}");
        }
    }

    /// What may surround an invocation without changing what the line is:
    /// where and when it runs, the message it is fed, how its output is
    /// trimmed, and how its failure is reported.
    #[test]
    fn an_invocation_keeps_its_meaning_through_setup_and_plumbing() {
        let sessions = Kind::Act(Act::Sessions);
        for line in [
            "timeout 20 cagents --device fw16 --no-start sessions --json",
            "cd /repo && timeout 20 cagents --device fw16 --no-start sessions --json 2>&1 | head -50",
            "set -euo pipefail; cagents sessions | jq -r '.[].id' | sort",
            "sleep 30; cagents sessions 2>/dev/null || echo 'daemon unreachable'",
            "cd /repo || exit 1; export CAGENTS_DEVICE=fw16; cagents sessions",
            "bash -lc 'cagents sessions | tail -5'",
            "cagents --json sessions | jq -r --arg d fw16 '.[] | select(.device == $d) | .id' | head -n 3",
            "cagents sessions 2>&1 | grep -E 'Relocate|Review' | cut -d ' ' -f 1",
            "cagents sessions | head --lines 5",
            "cagents sessions | grep -A3 -e Relocate -e Review",
            "cagents sessions | sort -k 2 | uniq -c",
            "cagents sessions | nl -w 3 -s ': '",
            "cagents sessions | column -t -N ID,TITLE -R ID",
            "cagents --json sessions | jq -L lib -r '.[].id'",
            "cagents sessions | rg -g '*.rs' -t rust -M 80 Relocate",
            "cagents sessions | rg -r X Relocate",
        ] {
            assert_eq!(
                cli(line, "").map(|step| step.kind),
                Some(sessions.clone()),
                "{line}"
            );
        }
        let fed = cli("cat <<'EOF' | cagents send parent -\nDone.\nEOF\n", "").unwrap();
        let Kind::Message(message) = fed.kind else {
            panic!("a message");
        };
        assert_eq!(message.body, Text::Inline("Done.\n".into()));
        // `cat` of a file into it is the file, as `<` gives it.
        let fed = cli("cat review.md | cagents send reviewer -", "").unwrap();
        let Kind::Message(message) = fed.kind else {
            panic!("a message");
        };
        assert_eq!(message.body, Text::File("review.md".into()));
        // A ticket body or a definition read from `-` takes what is piped.
        let commented = cli(
            "cat <<'EOF' | cagents linear comment add --current --body-file -\nLooks right.\nEOF\n",
            "",
        )
        .unwrap();
        assert!(
            matches!(
                commented.kind,
                Kind::Act(Act::Linear {
                    verb: "Commented on",
                    ..
                })
            ),
            "{:?}",
            commented.kind
        );
        assert!(
            cli(
                "cat automation.json | cagents automation create --definition -",
                ""
            )
            .is_some()
        );
    }

    #[test]
    fn claude_agents_and_messages_read_their_input() {
        let input =
            r#"{"description":"Review the PR","prompt":"Look at #12","subagent_type":"Explore"}"#;
        let started = native(Provider::Claude, "toolu_1", "Agent", input, "Looks good.").unwrap();
        assert_eq!(started.subagents, ["toolu_1"]);
        let Kind::Spawn(spawn) = &started.kind else {
            panic!("an agent call is a spawn");
        };
        assert!(spawn.waits);
        assert_eq!(
            (spawn.agents[0].kind.as_str(), &spawn.agents[0].prompt),
            ("Explore", &Text::Inline("Look at #12".into()))
        );
        assert_eq!(started.outcome.run.unwrap().result, "Looks good.");
        // Claude Code's usage trailer is not the report, and a report
        // handed back as a message is shown where it arrives.
        let trailed = "Looks good.\nagentId: a1 (use SendMessage with to: 'a1')\n<usage>total_tokens: 9</usage>";
        assert_eq!(claude_report(trailed).as_deref(), Some("Looks good."));
        let handed =
            "This agent's report was delivered to you as a message from \"a1\".\n\nagentId: a1";
        assert_eq!(claude_report(handed), None);

        let background = r#"{"prompt":"x","run_in_background":true}"#;
        let launched =
            native(Provider::Claude, "toolu_2", "Agent", background, "Launched").unwrap();
        assert!(launched.outcome.run.is_none());
        // Claude Code may background an agent it was not asked to; its
        // acknowledgement is not a report either.
        let acknowledged = "Async agent launched successfully. (This tool result is internal metadata.)\nagentId: a2 (internal ID)\nThe agent is working in the background.";
        let unasked = native(Provider::Claude, "toolu_5", "Agent", input, acknowledged).unwrap();
        assert!(unasked.outcome.run.is_none());
        assert!(matches!(
            unasked.kind,
            Kind::Spawn(Spawn { waits: false, .. })
        ));

        let sent = native(
            Provider::Claude,
            "toolu_3",
            "SendMessage",
            r#"{"to":"reviewer","message":"Also check docs","summary":"docs"}"#,
            "",
        )
        .unwrap();
        let Kind::Message(message) = sent.kind else {
            panic!("a message");
        };
        assert_eq!(
            (message.to.as_str(), message.summary.as_str()),
            ("reviewer", "docs")
        );
        assert!(!sent.cli, "a provider's own tool addresses its agents");
        assert!(native(Provider::Claude, "toolu_4", "Bash", "{}", "").is_none());
    }

    /// Claude's `SendMessage` names its recipient by agent ID or by name, and
    /// answers with JSON saying whether it went and which agent it reached.
    #[test]
    fn claude_messages_read_whom_they_reached_and_how_it_went() {
        let send = |to: &str, output: &str| {
            native(
                Provider::Claude,
                "toolu_send",
                "SendMessage",
                &serde_json::json!({"to": to, "message": "The repo moved", "summary": "repo moved"})
                    .to_string(),
                output,
            )
            .unwrap()
        };
        let queued = send(
            "a1892b93e88999a7d",
            r#"{"success":true,"message":"Message queued for delivery to a1892b93e88999a7d at its next tool round.","pin":{"id":"a1892b93e88999a7d","name":"a1892b93e88999a7d","ref":"68cf75"}}"#,
        );
        assert_eq!(queued.subagents, ["a1892b93e88999a7d"]);
        assert_eq!(queued.outcome.delivery, Some(Delivery::Queued));
        assert_eq!(queued.status(ToolStatus::Succeeded), ToolStatus::Succeeded);
        // A name resolves to the agent ID the result pinned.
        let named = send(
            "reviewer",
            r#"{"success":true,"message":"Resuming agent reviewer","revivedAgentId":"a0aac0bb27f14ac7e","pin":{"id":"a0aac0bb27f14ac7e","name":"reviewer","ref":"04aad4"}}"#,
        );
        assert_eq!(named.subagents, ["a0aac0bb27f14ac7e"]);
        assert_eq!(named.outcome.delivery, Some(Delivery::Resumed));
        // Claude reports a refusal as a successful call.
        let refused = send(
            "persist-ui-state",
            r#"{"success":false,"message":"No agent named 'persist-ui-state' is reachable.\nUse ListAgents to see everyone you can message."}"#,
        );
        assert_eq!(refused.status(ToolStatus::Succeeded), ToolStatus::Failed);
        assert_eq!(
            refused.outcome.error.as_deref(),
            Some("No agent named 'persist-ui-state' is reachable.")
        );
        assert_eq!(refused.subagents, ["persist-ui-state"]);
        // Until the result arrives, the recipient is as the call named it.
        assert_eq!(
            send("a1892b93e88999a7d", "").subagents,
            ["a1892b93e88999a7d"]
        );
        assert_eq!(send("a1892b93e88999a7d", "").outcome, Outcome::default());
        let inbox = send(
            "uds:/run/user/1000/cc-socks/1.sock",
            r#"{"success":true,"message":"Sent; in that session's inbox, not yet read by its Claude"}"#,
        );
        assert!(
            inbox.subagents.is_empty(),
            "another session is not an agent of this one"
        );
        assert_eq!(inbox.outcome.delivery, Some(Delivery::Inbox));
        assert!(send("*", "").subagents.is_empty());
        // A background agent's task is its agent ID.
        let stopped = native(
            Provider::Claude,
            "toolu_stop",
            "TaskStop",
            r#"{"task_id":"a1892b93e88999a7d"}"#,
            "",
        )
        .unwrap();
        assert_eq!(stopped.subagents, ["a1892b93e88999a7d"]);
    }

    #[test]
    fn codex_and_omp_calls_name_the_agents_they_reach() {
        let spawned = native(
            Provider::Codex,
            "call",
            "spawnAgent",
            "Write the tests",
            r#"{"thread-a":{"status":"running"}}"#,
        )
        .unwrap();
        assert_eq!(spawned.subagents, ["thread-a"]);
        assert!(spawned.is_card());
        let waited = native(Provider::Codex, "call", "wait", "", r#"{"a":{},"b":{}}"#).unwrap();
        assert_eq!(
            waited.kind,
            Kind::Act(Act::Wait {
                agents: vec!["a".into(), "b".into()]
            })
        );
        // The collaboration tools Codex added later read as what they do,
        // not as raw tool calls.
        let states = r#"{"thread-a":{"status":"running"}}"#;
        for (name, kind) in [
            ("sendMessage", OutboundKind::Send),
            ("followupTask", OutboundKind::Prompt),
        ] {
            let Some(Coordination {
                kind: Kind::Message(message),
                ..
            }) = native(Provider::Codex, "call", name, "Check the docs", states)
            else {
                panic!("{name} is a message");
            };
            assert_eq!(
                (message.kind, message.to.as_str()),
                (kind, "thread-a"),
                "{name}"
            );
        }
        assert_eq!(
            native(Provider::Codex, "call", "interruptAgent", "", states)
                .unwrap()
                .kind,
            Kind::Act(Act::Interrupt {
                agent: "thread-a".into()
            })
        );
        assert_eq!(
            native(Provider::Codex, "call", "listAgents", "", "{}")
                .unwrap()
                .kind,
            Kind::Act(Act::Agents)
        );
        // Their output is the agents' state map, which is not shown as text.
        assert!(
            native(Provider::Codex, "call", "interruptAgent", "", states)
                .unwrap()
                .output_is_agent_states()
        );
        assert!(
            !native(
                Provider::Codex,
                "call",
                "sendMessage",
                "Check the docs",
                states
            )
            .unwrap()
            .output_is_agent_states()
        );
        // `task` is OMP's launcher, and only OMP's.
        let input = r#"{"i":"Split the work","context":"Shared notes","tasks":[{"name":"Docs","agent":"task","task":"Write docs"},{"name":"Tests","agent":"task","task":"Write tests"}]}"#;
        assert!(native(Provider::Claude, "call", "task", input, "").is_none());
        let tasks = native(Provider::Omp, "call", "task", input, "").unwrap();
        assert_eq!(tasks.subagents, ["Docs", "Tests"]);
        let Kind::Spawn(spawn) = tasks.kind else {
            panic!("a task call is a spawn");
        };
        assert_eq!(
            (spawn.agents.len(), spawn.brief.as_str()),
            (2, "Shared notes")
        );
        // A job's name is the ID it is listed under, not what it goes by.
        assert!(spawn.agents.iter().all(|agent| agent.name.is_empty()));
    }
}
