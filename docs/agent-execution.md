# Hosted agent execution

`cagents agent` creates daemon-owned Claude, Codex, and Oh My Pi conversations without requiring a window. Each conversation has an agent ID; each accepted prompt has a separate run ID. Opening cagents later adopts the conversation into its specified project/channel without changing the window's current focus.

```sh
cagents agent spawn 'Review the current change and report concrete findings'
cagents agent spawn --provider codex --project /repo --directory /repo/.worktrees/review --wait \
  --prompt-file task.md
```

The prompt is the positional argument, `--prompt-file FILE`, or standard input (`-`, or piped in with no prompt given). `--provider` defaults to the calling session's provider; a CLI outside cagents must name one. `--directory` defaults to the `--project` path when one is given and otherwise to the current directory. `--project` (also `--repository`) names an open project by any directory inside it and defaults to the open project containing the agent's directory. The project must already be open in the daemon's project registry. `--channel` selects an existing channel owned by that repository; omitting it creates a channel in the same acceptance transaction. Account, project, and working-directory errors never fall back to a different target.

Scheduled callers can retain an approved directory descriptor. Provider startup uses that descriptor as its working directory, and execution-authorizing commands check that the configured path still identifies the approved directory. This is a dispatch-boundary identity check, not a filesystem sandbox: provider RPCs may resolve paths themselves. Replacing or removing the directory prevents new prompts and approvals but does not prevent interruption, denial, or cleanup.

`--account PROVIDER/system` selects the system configuration. A managed or linked account uses `PROVIDER/UUID`. Omitting the flag resolves and records the project default at acceptance. Later default changes do not change that accepted request's account. Model, permission, and reasoning-effort options are passed through the existing native launch policy before the first prompt:

| Option | Claude | Codex | Oh My Pi |
| --- | --- | --- | --- |
| `--model` | `--model` | New thread `model` | `--model` |
| `--permission-mode` | `--permission-mode` | New thread `approvalPolicy` (`--ask-for-approval`) | `--approval-mode` |
| `--reasoning-effort` | `--effort` | New thread `model_reasoning_effort` override (`--config`) | `--thinking` |

Permission values are provider-specific. A reasoning effort is the provider's own single-word level, such as `low` or `high`, and is passed as given; the automation editor offers only the levels the chosen model advertises. The command does not broaden permissions automatically. If omitted, the selected provider configuration remains authoritative. Follow-up prompts reuse the hosted conversation and preserve its current controls.

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

Every command takes an agent or run by its whole ID or any unique prefix of one, matched against the caller's open agents and their latest runs; an ambiguous prefix fails and names the candidates. An agent handle stands for its latest run, and a whole run ID outside that list names an older run. People read eight-character IDs; JSON always carries whole ones.

Human output is compact: one line per agent, and a run's state followed by the last 20 lines of its result (`--tail N`, or `--full`). `--json` before `agent` selects the regular CLI envelope. Spawn/send return `data.agent`, `data.run`, `data.created`, and `data.idempotency_key`, plus `data.timed_out` with `--wait`; status and single-handle wait return `data.agent`, `data.run`, and `data.timed_out`; a multi-handle wait returns `data.results` (each with `agent` and `run`) and `data.timed_out`; list returns `data.agents`; requests, answer, and interrupt return `data.requests`, the requests still pending.

`wait` returns once every handle has settled or is waiting for input, or with `--any` once one has. `--wait` on spawn or send does the same for the run just accepted. Both are bounded by `--timeout` seconds (60 by default) and report the last known state when it passes.

`requests` lists what an agent is waiting on: each request's ID, its actions, and its fields with their choices. `answer` sends one of the listed actions, with `--field FIELD=VALUE` repeated for several fields or values; the daemon rejects a request, action, or field the agent did not ask for. `interrupt` stops the active run. These act as the agent's owner, not as a person taking it over, so the agent stays under its owner's control. Secret field values the provider prefilled are never returned.

A run moves from `Accepted` to `Running`, may enter `Waiting`, and finishes as `Succeeded`, `Failed`, `Cancelled`, or `Interrupted`. Waiting returns promptly so a caller can route the approval/question to a person. It is not successful completion. The result contains provider-correlated text, usage, an optional diagnostic, and an explicit truncation flag. Text is bounded to 1 MiB; diagnostics to 16 KiB. Terminal output is never scraped to infer completion.

Acceptance and its dispatch claim commit to the daemon's SQLite registry with `synchronous=FULL` before spawning a provider or submitting a prompt. An identical request with the same caller and idempotency key returns the original IDs; it does not send the prompt again. Reusing a key for different content fails. The CLI generates a key when `--idempotency-key` is omitted and retries once with it, on a new connection, when the daemon's answer is lost; retry by hand with the identical request and the reported key.

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

Each entry carries the live session ID, provider, `Idle`/`Working`/`Waiting` state, working directory, checkout (repository or linked worktree), project, and, for delegated agents, the owner and latest run. `relation` places it relative to the caller: `Myself`, `Parent` (the session that owns the caller), `Child` (an agent the caller may control; one started by a CLI outside cagents is shown as a user agent, because every such caller shares it), or `Peer`. A plain shell appears only as the caller or as the owner of a listed agent. `whoami` reports the calling session, its relatives, and peers working in the same checkout, which are the sessions most likely to collide with its edits.

The caller's identity comes from its daemon-issued capability, never from an environment variable a process could set. A CLI outside cagents has no session of its own. Listing is read-only: the agent commands below remain restricted to an agent's owner. The lower-level `cagents terminal` commands are not: any process that can reach the daemon socket can already write to or close a terminal by ID, so treat a listed peer's terminal as someone else's work and coordinate through its owner. Session IDs address the live process; a relaunched or recovered conversation is listed under a new ID.

## Console surface

`--surface terminal` creates a real hosted terminal running a cagents console over the same native provider actor. It does not launch another provider process and does not emulate the provider's stock TUI. The console streams a bounded transcript, structured usage, run outcomes, approvals, and questions.

- Ordinary input submits a new durable run.
- `/interrupt` requests interruption through the native adapter.
- `/answer REQUEST_ID ACTION [ANSWERS_JSON]` answers only the explicit displayed request/action. The optional JSON is an array of `{"field":"FIELD_ID","values":["VALUE"]}` objects. Unlike `cagents agent answer`, console input is a person at the agent's controls, so it takes the agent over.
- `/quit` detaches the console; the provider remains hosted.
- Closing the terminal card follows normal active-work confirmation and retires the exact paired actor.

## Delegation and bundled skill

Fresh hosted sessions receive a daemon-issued, session-bound capability in `CAGENTS_AGENT_AUTHORITY`. It is transient process context, omitted from public recipes and redacted in diagnostics. Agents use the same CLI commands as people, but can only inspect/control children created under their live, proved session. A plain human CLI invocation without that variable owns a separate root scope. Passing a parent ID never grants authority. Do not copy, print, unset, or fabricate capabilities to bypass that scope.

The service limits each parent to 64 open children, 8 concurrent/uncertain runs, a maximum delegation depth of 8, and 32 concurrent/uncertain runs globally. Close finished conversations when they are no longer needed. Explicit retirement revokes the issuing capability; proof-changing adoption must validate the retained exact session lineage.

The packaged `cagents-agents` skill teaches this CLI workflow, and when to prefer the provider's own subagents and tasks over a separate hosted session. Before a new provider/terminal process starts, cagents installs it without replacing user content and prepends the matched CLI directory to that child's PATH. Claude uses the selected `CLAUDE_CONFIG_DIR/skills` (or `HOME/.claude/skills`); Codex and OMP use `HOME/.agents/skills`. Installation occurs under the account launch lease and never recreates a removed selected home. Existing identical content is reused. An unedited copy of an earlier bundled revision, recognized by its SHA-256 in a recorded allowlist, is upgraded by an atomic rename in the same directory when the file is a regular file and neither it nor its `cagents-agents` directory is a symbolic link. The provider root and its `skills` directory may be links, for example into a dotfiles repository. Any other content, including user edits and a linked `cagents-agents` directory or `SKILL.md`, is preserved and reported in daemon diagnostics with its path. A hard-linked `SKILL.md` is also preserved. The allowlist is append-only and ends with the current bundle; a unit test fails when `assets/skills/cagents-agents/SKILL.md` changes until its new hash is appended. Provider discovery opt-outs still apply.

These locations follow the providers' documented skill discovery: [Claude skills](https://code.claude.com/docs/en/skills), [Codex skills](https://learn.chatgpt.com/docs/build-skills), and [OMP skills](https://github.com/can1357/oh-my-pi/blob/main/docs/skills.md). The same installed instructions appear in cagents' native filesystem skill catalog and provider terminal sessions.
