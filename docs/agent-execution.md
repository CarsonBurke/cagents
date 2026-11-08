# Hosted agent execution

`cagents agent` creates daemon-owned Claude, Codex, and Oh My Pi conversations without requiring a window. Each conversation has an agent ID; each accepted prompt has a separate run ID. Opening cagents later adopts the conversation into its specified project/channel without changing the window's current focus.

```sh
cagents agent spawn 'Review the current change and report concrete findings'
cagents agent spawn --provider codex --project /repo --directory /repo/.worktrees/review --wait \
  --prompt-file task.md
```

The prompt is the positional argument, which may begin with `-`, or `--prompt-file FILE`, or standard input when the argument is a lone `-`. Standard input is never read unless asked for. `--provider` defaults to the calling session's provider; a CLI outside cagents must name one. `--project` (also `--repository`) names an open project by its name or by any directory inside it, and defaults to the open project containing the agent's directory. `--directory` defaults to the current directory when that is inside the chosen project, as in a worktree, and otherwise to the `--project` path (for a Git project named by name, its main checkout). An explicit `--directory` outside the chosen project is refused. The project must already be open in the daemon's project registry. `--channel` explicitly selects an existing channel owned by that repository. Without it, an authenticated caller reuses its exact shared worktree or channel view when that view belongs to the target project. Missing or ambiguous membership creates a channel in the same acceptance transaction and reports a warning in both human and JSON output. External callers retain fresh-channel behavior. Membership comes from daemon-owned pane descriptors or a delegated session's recorded placement, never window focus or cwd. A session placed in multiple distinct views has no default placement. `whoami --json` reports `view_scope`; a worktree view keeps `channel` null. Accepted agent records expose typed `placement`, while persisted older channel records remain readable. Account, project, and working-directory errors never fall back to a different target.

Scheduled callers can retain an approved directory descriptor. Provider startup uses that descriptor as its working directory, and execution-authorizing commands check that the configured path still identifies the approved directory. This is a dispatch-boundary identity check, not a filesystem sandbox: provider RPCs may resolve paths themselves. Replacing or removing the directory prevents new prompts and approvals but does not prevent interruption, denial, or cleanup.

`--account PROVIDER/system` selects the system configuration. A managed or linked account uses `PROVIDER/UUID`. Omitting the flag resolves and records the project default at acceptance. Later default changes do not change that accepted request's account. Model, permission, and reasoning-effort options are passed through the existing native launch policy before the first prompt:

| Option | Claude | Codex | Oh My Pi |
| --- | --- | --- | --- |
| `--model` | `--model` | New thread `model` | `--model` |
| `--permission-mode` | `--permission-mode` | New thread `approvalPolicy` (`--ask-for-approval`) | `--approval-mode` |
| `--reasoning-effort` | `--effort` | New thread `model_reasoning_effort` override (`--config`) | `--thinking` |

Permission values are provider-specific. A reasoning effort is the provider's own single-word level, such as `low` or `high`, and is passed as given; the automation editor offers only the levels the chosen model advertises. The command does not broaden permissions automatically. If omitted, the selected provider configuration remains authoritative. Follow-up prompts reuse the hosted conversation and preserve its current controls.

`--name reviewer` gives a new agent a durable handle unique among your open agents. Names start with a lowercase letter and contain up to 64 lowercase letters, digits, hyphens or underscores; whole UUIDs are reserved for IDs. Another owner may use the same name. Closing the agent releases its name for reuse. For example, spawn with `cagents agent spawn --name reviewer 'Review this change'`, then continue with `cagents agent send reviewer 'Check the tests too'`. List and JSON output include the name.

## Results and retries

```sh
cagents agent list
cagents agent status 1a2b --tail 40
cagents agent wait 1a2b 9f8e --any --timeout 300
cagents agent send 1a2b 'Investigate the first finding' --wait
cagents agent requests 1a2b
cagents agent answer 1a2b REQUEST_ID submit --field choice='review first'
cagents agent interrupt 1a2b
cagents agent close 1a2b
```

An interrupt may also stop the provider's background subagents, including one
waiting on its own background work (Claude does this). The daemon refuses that
interrupt unless you pass `--including-subagents`, and names the subagents it
would stop by their tasks. It checks the current tasks at provider dispatch, so a
background task that starts after your status check remains protected. To stop
just one task, use its subagent Stop action in chat instead. The provider console
uses `/interrupt` with the same safeguard; `/interrupt --including-subagents`
explicitly permits stopping the background work. Stopped background tasks do not
resume automatically. Closing an agent ends the entire owned conversation, and
the provider ends its background subagents as it quits.

Closing a chat, or stopping the daemon, ends its provider the way quitting the
provider's own CLI does, and cagents stops nothing more. Whatever the provider
leaves running keeps running, such as a server an agent started with `setsid`,
`nohup` or `tmux`. Claude Code stops the commands it was still running as it
quits; how Codex treats its running commands hasn't been checked. An update
that carries a chat over doesn't close it, so its processes keep running too.
These cases stop everything the provider started instead:
- an automation's agent, or one another session started, until a person takes
  part in it;
- a provider something outside cagents killed, such as the system for memory;
- a close that hands the chat elsewhere, such as returning it to its terminal,
  closing it with its paired terminal, or switching accounts.

Daemon Stop confirmation counts background subagents, running or waiting on their
own background work, as well as hosted sessions. If those tasks change before
confirmation is accepted, refresh the preview and confirm again. An update with
**When idle** waits for background work to finish. A subagent waiting on its own
background work, such as a dev server it started, may never finish while its
conversation looks done, so the preview names it: stop it from its tab, or
choose **Interrupt**, which previews which background tasks will stop. Automatic
cleanup of an automation's agent is refused for the same reason, naming the
subagent. New affected
tasks require fresh update confirmation.

Stopping the daemon does not close its agents. Each agent's provider and any
turn in progress end, and the stopped provider can no longer act for the agent,
but the agent stays open: `agent list` still shows it, and its card stays in its
channel, where its saved conversation can be resumed. A turn the stop cut short
ends with a note that it may be incomplete and is never resent, so inspect the
conversation before sending more work. Until the conversation is resumed, a new
prompt to the agent fails without being sent. `agent close` still retires a
stopped agent.

Every command takes an agent by its exact name, or an agent or run by its whole ID or any unique prefix of one, matched against the caller's open agents and their latest runs; an ambiguous prefix fails and names the candidates. An agent handle stands for its latest run, and a run handle for its agent. A whole ID outside that list is passed on as an agent, or to `wait` as an older run. Names resolve without case sensitivity and take precedence over ID prefixes. People read eight-character IDs; JSON always carries whole ones.

Human output is compact: one line per agent, and a run's state followed by the last 20 lines of its result (`--tail N`, or `--full`). A run's state describes one accepted prompt, so `agent list` and `agent status` also say what the provider is doing now, such as `idle; 2 background agents`: an agent with an idle turn takes a prompt straight away, whatever its background agents are doing. `cagents ls` reports sessions the same way, and its JSON `state` is the main turn with `background_agents` beside it. `--json` before `agent` selects the regular CLI envelope. Spawn/send return `data.agent`, `data.run`, `data.created`, and `data.idempotency_key`, plus `data.timed_out` with `--wait`; status returns `data.agent`, `data.run`, `data.provider` (the `turn` and `background_agents` of a live provider, otherwise null), and `data.timed_out`; wait always returns `data.results` (each with `agent`, `run`, and `outcome`), `data.timed_out`, `data.ended_by`, and the aggregate `data.outcome`; list returns `data.agents`; requests, answer, and interrupt return `data.requests`, the requests still pending.

`wait` returns once every handle has settled or is waiting for input, or with `--any` once one has. `--wait` on spawn or send does the same for the run just accepted. Both are bounded by `--timeout` seconds (60 by default) and report the last known state when it passes; a waiting run is polled until then, so one answered meanwhile is not reported stale.

Waiting commands use exit code **0** for success, **3** for timeout, **4** for failed, cancelled or interrupted work, and **5** for input, approval or an arrived message requiring attention. General errors remain **1** and usage errors **2**. This applies to `agent wait`, `agent spawn --wait`, `agent send --wait`, and `automation propose --wait` / `automation proposal --wait` (pending at the deadline is 3; declined is 4). `terminal wait` timeout also returns 3. An uncompleted wait never returns success. With `--any`, the first ready run in polling order determines the outcome. With all handles, failure outranks input/approval, which outranks timeout, which outranks success. Results retain each run's individual outcome.

`requests` lists what an agent is waiting on: each request's ID, its actions, and its fields with their choices. `answer` sends one of the listed actions, with `--field FIELD=VALUE` repeated for several fields or values; the daemon rejects a request, action, or field the agent did not ask for. Field values are command arguments, visible to other local processes, so do not pass secrets this way. `interrupt` stops whatever the provider is doing: the active run, a turn no run started (a task notification or a delivered message can start one), or, with `--including-subagents`, background subagents left running after the main turn ended. With nothing to stop, it says so. `answer` can approve only while the latest run is active, and can always decline. Both act as the agent's owner rather than as a person taking it over, so the agent stays under its owner's control. Once a person has taken the agent over in cagents or its console, they are refused: its turns and the questions they raise are that person's. Secret field values the provider prefilled are never returned.

`send` to an agent whose provider is busy with a turn no run owns does not fail: the run stays `Accepted` with `queued_ms` set, nothing is sent, and the prompt goes when that turn ends and any pending request is answered. There is no deadline. `interrupt` stops the turn and withdraws the queued prompt, leaving its run `Cancelled`, and reports which of those it did in `data.interrupted` and `data.withdrawn`. `interrupt --prompt-only` withdraws the prompt and interrupts nothing, for when the turn or its background agents are worth keeping. Once a person has taken the agent over, the owner's `interrupt` only withdraws the owner's prompt: the turn, its requests and any prompt the person sent are theirs. Stopping the turn from the chat withdraws the prompt too, and closing the agent cancels it. If the provider exits or the daemon restarts first, the run is `Failed` and says the prompt was not sent. A queued prompt is unfinished work, so an update is refused until it has been sent or withdrawn, and the refusal says how to withdraw it. A Claude agent can also report a background agent as waiting on background work of its own: status shows it in `provider.waiting_agents` and says its result so far may be interim, because that agent is alive and may report again. It doesn't hold back a send. A later report settles it, its resuming returns it to running, and stopping it or the provider exiting ends it as stopped; its parent's turn ending changes nothing, so it can stay waiting for as long as the provider runs if no report comes. Scheduled firings don't queue: they fail with a diagnostic and follow the automation's own policy.

A run moves from `Accepted` to `Running`, may enter `Waiting`, and finishes as `Succeeded`, `Failed`, `Cancelled`, or `Interrupted`. Waiting returns promptly so a caller can route the approval/question to a person. It is not successful completion. The result contains provider-correlated text, usage, an optional diagnostic, and an explicit truncation flag. Text is bounded to 1 MiB; diagnostics to 16 KiB. Terminal output is never scraped to infer completion.

Acceptance and its dispatch claim commit to the daemon's SQLite registry with `synchronous=FULL` before spawning a provider or submitting a prompt. An identical request with the same caller and idempotency key returns the original IDs; it does not send the prompt again. Reusing a key for different content fails. Because each commit waits on the disk, the CLI allows the daemon 30 seconds to answer a submission, rather than the 5 seconds it allows ordinary requests. The CLI generates a key when `--idempotency-key` is omitted and retries once with it, on a new connection, when the daemon's answer is lost. A submission that still fails reports its key in the error; retry by hand with the identical request and that key.

If a daemon restart loses proof of an accepted prompt's outcome, the run becomes `Interrupted`. Its external effects may already have happened. It is never resent automatically and retains its concurrency reservation until explicitly reconciled/closed. A private pending launch record preserves ownership if a process started but its final descriptor could not be committed. Such a record is not exposed as an adoptable UI session. A startup failure that retains a known hosted session reports that descriptor; inspect or close it before deciding how to proceed.

## Reading context and account budgets

```sh
cagents account list
cagents account list --provider codex --json
```

`account list` reads active accounts and any retained caller account from the
selected daemon. Its table shows stable account IDs, names, defaults, current
caller selection and archived status. JSON projects those metadata fields with
lowercase provider names; it omits usage bodies and launch recipes. This request
does not probe a provider CLI. A default marker describes saved preferences,
not a guarantee that the account is currently available for new work.

```sh
cagents account info --json
cagents account info --provider claude --json
cagents account info --account claude/system --refresh --json
```

An agent can read its latest reported context usage and the cached plan allowances of
its selected and other configured accounts. Caller identity is authenticated
by the daemon; selected account identity comes from the existing session's
captured effective execution environment, independent of the provider default for future chats. Native caller
usage reads resident metadata and never loads the conversation transcript.
Terminal caller context and selected account binding remain unknown: the
foreground provider can inherit bindings changed after the shell started, so a
PTY launch environment does not prove its current account. Cached allowances
for the configured accounts remain available. Context tokens are occupancy, separate from cumulative input
and output token counts. Occupancy can lag during a running turn; the daemon
query time does not establish when that occupancy was observed.

The default report is read-only and does not start provider probes. Explicit
refresh requires a single account ID and uses the existing isolated provider
usage query; it does not send a model prompt. The JSON report preserves source,
observation time, age, stale windows, elapsed resets, identity conflicts, and
unknown values. A selected archived account remains readable, marked archived,
without becoming eligible for new work. Plan remaining fractions use reported
rate limits; extra-usage balances and derived remaining money are separate.
API-equivalent token costs cannot establish how much subscription allowance
remains. See [account configuration](account-configuration.md#reading-usage-from-the-cli)
for the full report contract.

## Finding other sessions

`cagents sessions` (alias `ls`) lists the live provider conversations in the current project, whoever started them. That includes native chats, provider CLIs running in terminals, delegated agents, and automations. `--project PATH` selects another open project; `--all` lists every project; `--channel` lists the caller's channel (see [Reading your channel](#reading-your-channel)). A directory outside every open project lists everything, and the reply's `scope` says so.

```sh
cagents ls
cagents ls --all
cagents whoami
```

```text
7c0fba49-2f0e-4b8e-9d0a-1c2b3d4e5f60  claude idle     [cagents] /repo/.worktrees/fix  "Fix flaky login test" (my agent)
    agent 51c3e8a2-6d7f-4a1b-8c9d-0e1f2a3b4c5d, latest run Succeeded
3e81d0aa-9b8c-4d7e-a6f5-e4d3c2b1a090  codex  working  [cagents] /repo
```

Each entry carries the live session ID, provider, `Idle`/`Working`/`Waiting` state, working directory, checkout (repository or linked worktree), project, and, for delegated agents, the owner and latest run. `relation` places it relative to the caller: `Myself`, `Parent` (a session of the lineage that owns the caller), `Child` (an agent the caller may control; one started by a CLI outside cagents is shown as a user agent, because every such caller shares it), or `Peer`. A plain shell appears only as the caller or as the owner of a listed agent. `whoami` reports the calling session, its relatives, and peers working in the same checkout, which are the sessions most likely to collide with its edits.

The caller's identity comes from its daemon-issued capability, never from an environment variable a process could set. A CLI outside cagents has no session of its own. Listing is read-only: the agent commands below remain restricted to an agent's owner. Inside a hosted session the lower-level `cagents terminal` commands are restricted too. That includes a plain shell, since an agent may be running in it. They still list, snapshot and wait on any terminal, but `write` and `close` reach only the terminals the session's lineage controls: its own session, the terminals it started with `cagents terminal spawn`, and the sessions and consoles of the agents it delegated to. To reach anyone else's, message its owner. The daemon records which lineage started each terminal, from registry schema version 6, so this survives a reconnect and a daemon restart; a terminal relaunched in its card keeps its owner. A spawn from inside a session may start a new terminal but not relaunch an existing one. The check applies to the connection the CLI authenticates with the session's capability, which also refuses everything outside the terminal surface. Like a CLI outside cagents, a process that reaches the daemon socket without authenticating acts for the user. So does a command run with `CAGENTS_AGENT_AUTHORITY` unset, which a person at a hosted shell can use to act as themselves. Session IDs address the live process; a relaunched or recovered conversation is listed under a new ID. Its `lineage`, printed beneath a session whose lineage differs from its ID, names the root session it succeeds and stays the same across those relaunches and recoveries.

## Reading your channel

`cagents ls --channel` lists the sessions in the caller's channel, or in `--channel CHANNEL`. It includes native chats whose provider has exited, for example after a usage limit, until their card is closed, and lists the most recently active first, with when each conversation was last saved. With `--json` it returns `data.channel`, plus `data.me` (when the caller is in that channel) and `data.sessions`. Each member holds its directory entry as `session`, with `exited`, `failure` and `active_at` (Unix milliseconds). `whoami` marks the peers that share the caller's channel.

A session its provider refused at an account usage limit is listed as `limited` (`Limited` in JSON) rather than idle, with the limit as `limit`: its `summary`, the provider's own `reset` wording and, when the account's usage readings date it, `resets_at` (Unix seconds). The daemon judges the limit from the provider's last error. A reading taken after the refusal dates it by the latest reset among the account's used-up windows. The limit lifts when that reset passes, when a reading taken after the refusal shows allowance again, or when the conversation's own next turn is accepted; background subagents still working do not lift it. Without a dating reading, the limit lasts until the conversation continues. Each live session also carries `account`: the provider account it runs on, its usage status, the allowance left in each window with its reset, and whether that reading is stale. `ls`, `whoami` and `agent status` print both, plain and in JSON, and the app marks a limited conversation's tab and project with an alarm badge.

`cagents transcript SESSION` (alias `history`) reads a page of another session's conversation from the provider's own saved history: Claude's project transcript or Codex's rollout, including an archived one. It names sessions as `cagents send` does. The daemon locates the file from that session's own launch: provider, conversation, working directory and account home. Callers never pass a path. Reading neither starts the provider nor requires it to be running, and it grants no control, like listing.

```sh
cagents ls --channel
cagents transcript 7c0f
cagents transcript 7c0f --before 3f2a1b7c-0d9e-4c8b-a7f6-5e4d3c2b1a00
cagents transcript 7c0f --tools --full --json
```

```text
Session 7c0fba49-2f0e-4b8e-9d0a-1c2b3d4e5f60 (claude, exited), active 12 min ago
Failed: Claude usage limit reached · resets 3pm
Earlier: cagents transcript 7c0f --before 3f2a1b7c-0d9e-4c8b-a7f6-5e4d3c2b1a00

[user]
  The login test fails one run in ten; find out why.

[tools] Run ×9, Read ×4, Edit ×2; edited tests/login.rs, src/auth.rs

[assistant]
  The session cookie expires during the retry loop. I extended its lifetime in the fixture and ...
```

A page holds the last `--limit` messages (10 by default, at most 200): user messages, assistant text, messages delivered by other sessions and reports from subagents. Notices between them, including Claude's compaction summaries and Codex's compaction markers, come along. Each run of consecutive tool calls folds into one line with counts, failures and edited paths; `--tools` lists every call with clipped output instead. Messages are clipped to 2,000 bytes and notices to 600, with the omitted size shown; `--full` keeps them whole, except that a Codex command's output is kept to 64 KiB. The page lists oldest first, so the newest message is last. Each entry's text is indented under its label, so text that quotes another transcript cannot read as an entry. A failure is summarized in one line. A listing of one channel, and a transcript's heading, show only a session's own title, never its channel's. Its `Earlier:` line gives the command for the previous page, with the same options. `--json` returns the session as a channel listing reports it, the entries with their IDs and the `earlier` cursor. A forked Codex thread includes the history it replays from its parent; Codex's own instructions and environment context are left out. Reasoning never appears, and tool output appears only with `--tools`. A provider CLI in a terminal is read from the conversation it is resuming or was last seen in. Omp conversations cannot be read yet.

## Messages between sessions

Any session, or the user from their own shell, can leave a message for another live session. The message is addressed to the recipient's lineage, so it still arrives if that session is relaunched or recovered before reading it.

```sh
cagents send 7c0f 'I am refactoring src/auth.rs; hold edits there until I report back'
cagents send parent 'Blocked: the staging credentials are missing'
cagents ask 7c0f 'Which branch are you on?' --timeout 300
cagents reply 9a41 'main'
cagents inbox
```

- `send SESSION` names the recipient by at least four characters of its session ID, lineage, or delegated agent ID, as `cagents ls` prints them. `parent` is the session that started the caller as its agent, or the user when the user's own CLI started it. A session cannot message itself.
- `ask` sends a question and waits for the reply, 90 seconds by default and at most an hour. Until then the reply is held for the waiting command rather than delivered to the asker's conversation; once the wait ends, a later reply is delivered like any other message. A question to a lineage with no live session does not wait, since no one can answer it yet. `reply MESSAGE_ID` answers a message the caller received, by any prefix of its ID; only the recipient's lineage may answer it, and `--wait` waits for the answer in turn.
- `inbox` prints a batch of the caller's oldest undelivered messages and marks received messages read. A batch contains up to 64 messages and fits within one reply; repeat `inbox` to drain larger queues. `--all` lists up to the 50 most recent, delivered or not, and `--wait` blocks until one arrives. Messages to the user are read this way from a shell outside cagents. If a receipt cannot be confirmed, a received message may appear again with the same ID.
- An agent blocked in `cagents agent wait`, or in `spawn`/`send --wait`, is mid-turn, so the courier cannot reach it. Its wait therefore also returns when a message arrives for it, prints the message, and marks it read; the agent answers and waits again. A child can thus `ask parent` while its parent waits on it. The user's own shell has no such turn, so its waits are unchanged. With `--json`, a wait reports `ended_by` as `runs`, `messages` or `timeout`; only `runs` means the runs are ready.

The daemon's courier gives a live native conversation its oldest waiting message as a prompt once the conversation is idle: ready, not working, and not waiting on a question or approval. It never interrupts a turn, and the next message waits for the turn the previous one started to finish. It prompts as an execution client, so delivering a message never takes a delegated agent over. If the conversation refuses the prompt, usually because a person's own prompt reached it first, the message returns to the queue. Delivery is at most once: a prompt the conversation took but did not acknowledge in time, a provider that rejects a turn after accepting the prompt, or a daemon that stops between claiming a message and prompting leaves the message marked delivered rather than risk a second prompt. A session running in a terminal, such as a provider CLI, is never prompted; it reads its messages with `cagents inbox`. A message to a lineage with no live session waits for the next one to start.

The courier does not prompt a conversation stopped by a usage limit. Its messages stay pending until the limit lifts, and the courier looks again when a dated limit resets. `send` and `ask` say so beneath their `Sent` line, and give the recipient's session and the `notice` in JSON. `ask` does not wait for a limited recipient; a later reply arrives as a message. `agent send` refuses an agent stopped until a known reset with the error `agent_limited`, and sends nothing; a retry of a submission the daemon already accepted still returns that acceptance. When the reset is unknown, `agent send` sends the prompt, since a new turn shows whether the limit still holds, and adds a `notice`.

For a live terminal running a provider CLI, unread mail creates separate **Session messages waiting** attention in the app. Desktop delivery follows the existing Waiting notification preference and focus policy. The alert contains only fixed instructions: no message body, sender title, directory, URL or authority token. Opening it resolves the current, unambiguous terminal for the recipient's lineage and focuses only an open pane with the same launch and live host proof. A closed, replaced or ambiguous recipient leaves the inbox unread and reports that it is unavailable here. It never types into the PTY, prompts the provider, or starts a daemon.

Notification acknowledgement and mailbox receipts are independent. Focusing or dismissing an alert leaves messages unread; `cagents inbox` reads them explicitly. Arrivals coalesce per lineage without replacing lifecycle or review attention. A new message can reactivate dismissed inbox attention; repeated reconciliation, partial reads and daemon restart preserve the same arrival identity and dismissal. Held ask replies become alert-eligible when their hold ends, while older available mail remains eligible. A held reply that was explicitly read before release never creates an alert later. Releasing an earlier queued hold is not a new arrival: dismissing a later coalesced alert also covers earlier queued replies, so their release does not redisplay that alert. When all pending mail is read, delivered or expires, obsolete inbox attention is retired. An offline lineage keeps existing attention but receives no new desktop alert until one live provider terminal owns it; native chats retain their idle prompt delivery.


A delivered message is a prompt framed for the agent:

```text
[cagents message from session 3e81d0aa "Fix flaky login test"]
I am refactoring src/auth.rs; hold edits there until I report back
[Reply: cagents reply 9a41c2e7 "…" · seal 5f0c…]
```

The daemon seals each frame with a keyed hash under a secret it keeps in its registry. Windows show a sealed prompt as a message from its sender, both when it is delivered and when the transcript is read back from provider history; a prompt that only looks like one stays the user's own words. The seal proves who wrote a frame, not what its sender may ask for: a message is as trustworthy as a prompt from the session that sent it, and the recipient decides what to do with it. Nor does it bind a frame to the conversation it was delivered to; a sealed frame pasted into another conversation shows there as the same message. A caller without a daemon-issued capability is the user, so a process that drops its capability speaks as the user, as any program the user runs can.

A message body is at most 64 KiB, and a lineage holds at most 64 undelivered messages. The user reads messages only when they choose to, so the cap there applies to each sender separately. The courier deletes messages 30 days after they were sent, checking hourly. The daemon registry keeps messages from schema version 5.

## Console surface

`--surface terminal` creates a real hosted terminal running a cagents console over the same native provider actor. It does not launch another provider process and does not emulate the provider's stock TUI. The console prints what the agent said and one line per finished step, plus a line for any step still running after a few seconds; command output, tool payloads and delivered reports are left out. It also prints run outcomes with their token usage, approvals, and questions. A line appears once it is complete, and a step that finishes after later text was printed appears as one new line, so the transcript is not reprinted.

- Ordinary input submits a new durable run.
- `/interrupt` requests interruption through the native adapter. It refuses an interruption that would stop running background subagents; `/interrupt --including-subagents` permits that wider impact. A successful interrupt takes the agent over; a refusal leaves its owner in control.
- `/answer REQUEST_ID ACTION [ANSWERS_JSON]` answers only the explicit displayed request/action. The optional JSON is an array of `{"field":"FIELD_ID","values":["VALUE"]}` objects. Unlike `cagents agent answer`, console input is a person at the agent's controls, so it takes the agent over.
- `/quit` detaches the console; the provider remains hosted.
- Closing the terminal card follows normal active-work confirmation and retires the exact paired actor.

## Delegation and bundled skill

Fresh hosted sessions receive a daemon-issued, session-bound capability in `CAGENTS_AGENT_AUTHORITY`. It is transient process context, omitted from public recipes and redacted in diagnostics. Agents use the same CLI commands as people, but can only inspect/control children created under their live, proved session. A plain human CLI invocation without that variable owns a separate root scope. Passing a parent ID never grants authority. Do not copy, print, unset, or fabricate capabilities to bypass that scope.

A session's children belong to its lineage, not to its process. A successor joins the lineage and keeps control of the children when the session is relaunched in its card, recovered after a crash, reopened after its native owner's close was proved, recovered under another account, or converted between chat and terminal. It joins only if no other session of that lineage is still running, which account recovery, reopening and conversion prove for the session they replace by stopping it first; otherwise, as with a rival relaunch of a live session, it starts a lineage of its own, and the daemon logs a warning. Each successor joins the lineage of the exact session it replaces, so a rival's own agents pass to its successors in turn; a relaunch that failed to start passes on the lineage of the chain's first session. Resuming a closed conversation from history, branching a conversation, or cloning a pane starts a new lineage, except that a Codex conversation kept by its native server keeps its lineage. The lineage is recorded before the successor starts and never changes afterwards. Every live session of a lineage controls its children; the check above keeps that to one session at a time. A delegated agent's `owner` names its lineage, which is the ID of the session that began it and may no longer be live. Chats replaced before this release cannot be traced to their original lineage. The daemon registry records lineages from schema version 4, and the application state records each relaunched terminal's predecessor from schema version 11; an older cagents refuses to open either.


The service limits each parent lineage and each automation to 64 open children. That child quota does not limit conversations started directly by the user. Every origin, including the user, is limited to 8 concurrent/uncertain runs, with 32 concurrent/uncertain runs globally and a maximum delegation depth of 8. Close finished conversations when they are no longer needed. Explicit retirement revokes the issuing capability; proof-changing adoption must validate the retained exact session lineage.

The packaged `cagents` skill teaches this CLI workflow, and when to prefer the provider's own subagents and tasks over a separate hosted session. Before a new provider/terminal process starts, cagents installs it without replacing user content and prepends the matched CLI directory to that child's PATH. Claude uses the selected `CLAUDE_CONFIG_DIR/skills` (or `HOME/.claude/skills`); Codex and OMP use `HOME/.agents/skills`. Installation occurs under the account launch lease and never recreates a removed selected home.

Existing identical content is reused. An unedited copy of an earlier bundled revision, recognized by its SHA-256 in a recorded allowlist, is upgraded by an atomic rename in the same directory when the file is a regular file and neither it nor its `cagents` directory is a symbolic link. The provider root and its `skills` directory may be links, for example into a dotfiles repository. Any other content, including user edits and a linked `cagents` directory or `SKILL.md`, is preserved and reported with its path only in explicitly enabled local debug output. A hard-linked `SKILL.md` is also preserved. The allowlist is append-only and ends with the current bundle; a unit test fails when `assets/skills/cagents/SKILL.md` changes until its new hash is appended.

A personal `cagents` skill takes precedence over the bundled fallback. Managed Claude accounts link their skill directories to the system home, and Codex uses the process home’s shared `.agents/skills`, so edits to personal skills remain shared across accounts. At launch, managed Claude homes that predate the personal skill directory adopt its live link if their local skills directory is absent, empty, or contains only recognized fallback bundles; account-specific skills and existing links are preserved.

Launch also removes the retired `cagents-agents` bundle only when its directory contains nothing but a regular, single-link `SKILL.md` matching a recorded legacy hash; edited copies, links, and additional resources are preserved. Provider discovery opt-outs still apply.

These locations follow the providers' documented skill discovery: [Claude skills](https://code.claude.com/docs/en/skills), [Codex skills](https://learn.chatgpt.com/docs/build-skills), and [OMP skills](https://github.com/can1357/oh-my-pi/blob/main/docs/skills.md). The same installed instructions appear in cagents' native filesystem skill catalog and provider terminal sessions.

## CLI compatibility changes

Agent waits now always return `data.results`, including a single handle; consumers
that previously read `data.agent` / `data.run` for a one-handle wait should read
`data.results[0].agent` / `data.results[0].run`. Each result includes an `outcome`;
a run still active when `--any` returns has `pending`, while an active run at the
deadline has `timed_out`. Waiting commands now return the nonzero exit codes
above while retaining their normal JSON success envelope and last known results.
The envelope's `ok` means the command was processed; use the exit code or
`data.outcome` to determine whether the work completed successfully.

If the deadline expires before a run’s first status arrives, `agent wait` keeps
any statuses already received and reports the unread IDs in `data.unresolved_runs`.
Spawn/send retain their durable acceptance when that first status is unavailable.
No run outcome is invented from missing status evidence.

## Reads without startup

Add the global `--no-start` flag, or set `CAGENTS_NO_START=1`, when a read should
connect only to an already-running daemon. Missing local and selected-device
daemons produce a `daemon_not_running` error instead of starting services.
Transport failures remain errors and are distinguished from daemon absence.

`cagents daemon stop` never starts a daemon. When the selected daemon is absent,
it exits successfully; JSON output contains `data.state = "already_stopped"`.

## Shell completions

Generate completions from the installed command grammar. This does not contact
or start a daemon:

```sh
cagents completions bash > ~/.local/share/bash-completion/completions/cagents
cagents completions zsh > ~/.zfunc/_cagents
cagents completions fish > ~/.config/fish/completions/cagents.fish
```

Create the destination directory first. Bash and fish load their completion files
through the usual shell setup; add `~/.zfunc` to zsh's `fpath` before `compinit`.
Elvish and PowerShell scripts are available with `completions elvish` and
`completions powershell`. Regenerate after upgrading the CLI.

Automation definition and history pages accept `--limit` from 1 through 100;
values outside that range are refused. For definitions, pass the last returned
UUID as `--after` to request the next page.

Linear collection commands use the same plain tables and explicit empty states.
Their `--json` payloads retain the original API fields and pagination metadata.
Human pages indicate when more results are available; use `--after` to continue.
Comments appear in a single table row; use JSON to preserve Markdown line breaks.

Session directory tables keep full session IDs, lineage, view and delegation
context. Session and whoami JSON retain their original directory metadata.

## Terminal inventory output

`cagents terminal list` shows running terminals with a header, observed native
terminal titles and working directories. `--all` includes retained non-running
entries; `--exited` selects entries with a recorded exit. Missing exit evidence
is shown as “relaunch available”, not inferred to mean hibernated or successful.
Titles stay unknown until the daemon observes a terminal title event.

List JSON now uses a metadata-only `data.terminals` array with `id`, `state`,
`title`, `attached`, `program`, `working_directory` and `exit` fields, plus the
sampled daemon `proof` once at the top level. State names are snake_case.
Scripts that previously read `data.terminals[].session.id` should read
`data.terminals[].id`. Full launch environments and recovery descriptors remain
available through `cagents terminal show ID --json`; list output omits them.
