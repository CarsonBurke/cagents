# Hosted agent execution

`cagents agent` creates daemon-owned Claude, Codex, and Oh My Pi conversations without requiring a window. Each conversation has an agent ID; each accepted prompt has a separate run ID. Opening cagents later adopts the conversation into its specified project/channel without changing the window's current focus.

```sh
cagents agent spawn 'Review the current change and report concrete findings'
cagents agent spawn --provider codex --project /repo --directory /repo/.worktrees/review --wait \
  --prompt-file task.md
```

The prompt is the positional argument, which may begin with `-`, or `--prompt-file FILE`, or standard input when the argument is a lone `-`. Standard input is never read unless asked for. `--provider` defaults to the calling session's provider; a CLI outside cagents must name one. `--project` (also `--repository`) names an open project by its name or by any directory inside it, and defaults to the open project containing the agent's directory. `--directory` defaults to the current directory when that is inside the chosen project, as in a worktree, and otherwise to the `--project` path (for a Git project named by name, its main checkout). An explicit `--directory` outside the chosen project is refused. The project must already be open in the daemon's project registry. `--channel` selects an existing channel owned by that repository; omitting it uses the calling session's channel when that channel belongs to the target project, otherwise it creates a channel in the same acceptance transaction. Membership comes from daemon-owned pane descriptors or a delegated session's recorded placement, never window focus or cwd. A session placed in multiple channels has no default channel. Account, project, and working-directory errors never fall back to a different target.

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

An interrupt may also stop the provider's running background subagents (Claude
does this). The daemon refuses that interrupt unless you pass
`--including-subagents`. It checks the current tasks at provider dispatch, so a
background task that starts after your status check remains protected. To stop
just one task, use its subagent Stop action in chat instead. The provider console
uses `/interrupt` with the same safeguard; `/interrupt --including-subagents`
explicitly permits stopping the background work. Stopped background tasks do not
resume automatically. Closing an agent stops the entire owned conversation and
its background tasks.

Daemon Stop confirmation counts running background subagents as well as hosted
sessions. If those tasks change before confirmation is accepted, refresh the
preview and confirm again. An update with **When idle** waits for background work
to finish; **Interrupt** previews which background tasks will stop. New affected
tasks require fresh update confirmation.

Every command takes an agent by its exact name, or an agent or run by its whole ID or any unique prefix of one, matched against the caller's open agents and their latest runs; an ambiguous prefix fails and names the candidates. An agent handle stands for its latest run, and a run handle for its agent. A whole ID outside that list is passed on as an agent, or to `wait` as an older run. Names resolve without case sensitivity and take precedence over ID prefixes. People read eight-character IDs; JSON always carries whole ones.

Human output is compact: one line per agent, and a run's state followed by the last 20 lines of its result (`--tail N`, or `--full`). `--json` before `agent` selects the regular CLI envelope. Spawn/send return `data.agent`, `data.run`, `data.created`, and `data.idempotency_key`, plus `data.timed_out` with `--wait`; status and single-handle wait return `data.agent`, `data.run`, and `data.timed_out`; a multi-handle wait returns `data.results` (each with `agent` and `run`) and `data.timed_out`; list returns `data.agents`; requests, answer, and interrupt return `data.requests`, the requests still pending.

`wait` returns once every handle has settled or is waiting for input, or with `--any` once one has. `--wait` on spawn or send does the same for the run just accepted. Both are bounded by `--timeout` seconds (60 by default) and report the last known state when it passes; a waiting run is polled until then, so one answered meanwhile is not reported stale.

`requests` lists what an agent is waiting on: each request's ID, its actions, and its fields with their choices. `answer` sends one of the listed actions, with `--field FIELD=VALUE` repeated for several fields or values; the daemon rejects a request, action, or field the agent did not ask for. Field values are command arguments, visible to other local processes, so do not pass secrets this way. `interrupt` stops the active run. Both act only while the agent's latest run is active, and as its owner rather than as a person taking it over, so the agent stays under its owner's control. Once a person has taken the agent over in cagents or its console, they are refused: its turns and the questions they raise are that person's. Secret field values the provider prefilled are never returned.

A run moves from `Accepted` to `Running`, may enter `Waiting`, and finishes as `Succeeded`, `Failed`, `Cancelled`, or `Interrupted`. Waiting returns promptly so a caller can route the approval/question to a person. It is not successful completion. The result contains provider-correlated text, usage, an optional diagnostic, and an explicit truncation flag. Text is bounded to 1 MiB; diagnostics to 16 KiB. Terminal output is never scraped to infer completion.

Acceptance and its dispatch claim commit to the daemon's SQLite registry with `synchronous=FULL` before spawning a provider or submitting a prompt. An identical request with the same caller and idempotency key returns the original IDs; it does not send the prompt again. Reusing a key for different content fails. The CLI generates a key when `--idempotency-key` is omitted and retries once with it, on a new connection, when the daemon's answer is lost. A submission that still fails reports its key in the error; retry by hand with the identical request and that key.

If a daemon restart loses proof of an accepted prompt's outcome, the run becomes `Interrupted`. Its external effects may already have happened. It is never resent automatically and retains its concurrency reservation until explicitly reconciled/closed. A private pending launch record preserves ownership if a process started but its final descriptor could not be committed. Such a record is not exposed as an adoptable UI session. A startup failure that retains a known hosted session reports that descriptor; inspect or close it before deciding how to proceed.

## Finding other sessions

`cagents sessions` (alias `ls`) lists the live provider conversations in the current project, whoever started them. That includes native chats, provider CLIs running in terminals, delegated agents, and automations. `--project PATH` selects another open project; `--all` lists every project. A directory outside every open project lists everything, and the reply's `scope` says so.

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

A delivered message is a prompt framed for the agent:

```text
[cagents message from session 3e81d0aa "Fix flaky login test"]
I am refactoring src/auth.rs; hold edits there until I report back
[Reply: cagents reply 9a41c2e7 "…" · seal 5f0c…]
```

The daemon seals each frame with a keyed hash under a secret it keeps in its registry. Windows show a sealed prompt as a message from its sender, both when it is delivered and when the transcript is read back from provider history; a prompt that only looks like one stays the user's own words. The seal proves who wrote a frame, not what its sender may ask for: a message is as trustworthy as a prompt from the session that sent it, and the recipient decides what to do with it. Nor does it bind a frame to the conversation it was delivered to; a sealed frame pasted into another conversation shows there as the same message. A caller without a daemon-issued capability is the user, so a process that drops its capability speaks as the user, as any program the user runs can.

A message body is at most 64 KiB, and a lineage holds at most 64 undelivered messages. The user reads messages only when they choose to, so the cap there applies to each sender separately. The courier deletes messages 30 days after they were sent, checking hourly. The daemon registry keeps messages from schema version 5.

## Console surface

`--surface terminal` creates a real hosted terminal running a cagents console over the same native provider actor. It does not launch another provider process and does not emulate the provider's stock TUI. The console streams a bounded transcript, structured usage, run outcomes, approvals, and questions.

- Ordinary input submits a new durable run.
- `/interrupt` requests interruption through the native adapter. It refuses an interruption that would stop running background subagents; `/interrupt --including-subagents` permits that wider impact. A successful interrupt takes the agent over; a refusal leaves its owner in control.
- `/answer REQUEST_ID ACTION [ANSWERS_JSON]` answers only the explicit displayed request/action. The optional JSON is an array of `{"field":"FIELD_ID","values":["VALUE"]}` objects. Unlike `cagents agent answer`, console input is a person at the agent's controls, so it takes the agent over.
- `/quit` detaches the console; the provider remains hosted.
- Closing the terminal card follows normal active-work confirmation and retires the exact paired actor.

## Delegation and bundled skill

Fresh hosted sessions receive a daemon-issued, session-bound capability in `CAGENTS_AGENT_AUTHORITY`. It is transient process context, omitted from public recipes and redacted in diagnostics. Agents use the same CLI commands as people, but can only inspect/control children created under their live, proved session. A plain human CLI invocation without that variable owns a separate root scope. Passing a parent ID never grants authority. Do not copy, print, unset, or fabricate capabilities to bypass that scope.

A session's children belong to its lineage, not to its process. A successor joins the lineage and keeps control of the children when the session is relaunched in its card, recovered after a crash, reopened after its native owner's close was proved, recovered under another account, or converted between chat and terminal. It joins only if no other session of that lineage is still running, which account recovery, reopening and conversion prove for the session they replace by stopping it first; otherwise, as with a rival relaunch of a live session, it starts a lineage of its own, and the daemon logs a warning. Each successor joins the lineage of the exact session it replaces, so a rival's own agents pass to its successors in turn; a relaunch that failed to start passes on the lineage of the chain's first session. Resuming a closed conversation from history, branching a conversation, or cloning a pane starts a new lineage, except that a Codex conversation kept by its native server keeps its lineage. The lineage is recorded before the successor starts and never changes afterwards. Every live session of a lineage controls its children; the check above keeps that to one session at a time. A delegated agent's `owner` names its lineage, which is the ID of the session that began it and may no longer be live. Chats replaced before this release cannot be traced to their original lineage. The daemon registry records lineages from schema version 4, and the application state records each relaunched terminal's predecessor from schema version 11; an older cagents refuses to open either.


The service limits each parent lineage to 64 open children, 8 concurrent/uncertain runs, a maximum delegation depth of 8, and 32 concurrent/uncertain runs globally. Close finished conversations when they are no longer needed. Explicit retirement revokes the issuing capability; proof-changing adoption must validate the retained exact session lineage.

The packaged `cagents-agents` skill teaches this CLI workflow, and when to prefer the provider's own subagents and tasks over a separate hosted session. Before a new provider/terminal process starts, cagents installs it without replacing user content and prepends the matched CLI directory to that child's PATH. Claude uses the selected `CLAUDE_CONFIG_DIR/skills` (or `HOME/.claude/skills`); Codex and OMP use `HOME/.agents/skills`. Installation occurs under the account launch lease and never recreates a removed selected home. Existing identical content is reused. An unedited copy of an earlier bundled revision, recognized by its SHA-256 in a recorded allowlist, is upgraded by an atomic rename in the same directory when the file is a regular file and neither it nor its `cagents-agents` directory is a symbolic link. The provider root and its `skills` directory may be links, for example into a dotfiles repository. Any other content, including user edits and a linked `cagents-agents` directory or `SKILL.md`, is preserved and reported in daemon diagnostics with its path. A hard-linked `SKILL.md` is also preserved. The allowlist is append-only and ends with the current bundle; a unit test fails when `assets/skills/cagents-agents/SKILL.md` changes until its new hash is appended. Provider discovery opt-outs still apply.

These locations follow the providers' documented skill discovery: [Claude skills](https://code.claude.com/docs/en/skills), [Codex skills](https://learn.chatgpt.com/docs/build-skills), and [OMP skills](https://github.com/can1357/oh-my-pi/blob/main/docs/skills.md). The same installed instructions appear in cagents' native filesystem skill catalog and provider terminal sessions.
