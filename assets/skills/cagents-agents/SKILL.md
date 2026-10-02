---
name: cagents-agents
description: Start daemon-hosted Claude, Codex, or Oh My Pi sessions, find the other live sessions in a project, message them, and oversee agents you started through the cagents CLI. Use when work needs a separate session (another provider, another project, or one that outlives this conversation), when coordinating with other sessions working nearby, or when a message from another session arrives.
metadata:
  cagents-bundle-version: "3"
---

## Choose the lightest tool

- Work that belongs to this conversation: use your own harness's subagents, tasks, and background commands. They are cheaper and report straight back to you.
- Work that needs its own session: a different provider, another open project, a conversation the user should see and be able to take over in cagents, or one that must outlive yours. Use `cagents agent`.
- Other sessions already running: `cagents whoami` shows your session, its relatives, and peers in your checkout; `cagents ls` lists the project's live sessions (`--project PATH` for another project, `--all` for every one). Check before editing a checkout someone else is working in. Listing is read-only: you control only agents you started.
- Coordinating with another session: message it (below) rather than editing around it or guessing what it is doing.

## Start and oversee agents

```sh
cagents agent spawn 'Concrete task and completion criteria'      # same provider, current directory
cagents agent spawn --provider codex --project other-project --wait 'Task'   # project by name or path
cagents agent list                       # your open agents, with short IDs
cagents agent status 1a2b                # latest run and the end of its result; --full for all of it
cagents agent wait 1a2b 9f8e --timeout 300   # until all settle or ask for input; --any for the first
cagents agent send 1a2b 'Follow-up task' --wait
cagents agent close 1a2b
```

- The provider defaults to yours, the project to the one you are in, and the directory to the current one when it is in that project (otherwise pass `--directory`). The project must already be open in cagents. Optional: `--channel ID`, `--account PROVIDER/system|PROVIDER/UUID`, `--model`, `--permission-mode`, `--reasoning-effort`, `--surface terminal`. Do not weaken permissions to make unattended work proceed. Ask the user rather than guessing a repository, account, or policy.
- A prompt can also come from `--prompt-file FILE`, or standard input with `-`. Quote it so the shell passes it as one argument.
- Handles are agent or run IDs, or any unique prefix of one. An agent stands for its latest run. Add `--json` before `agent` for structured output: `data.agent`, `data.run` (`state`, `result.text`, `usage`, `truncated`, `diagnostic`), `data.timed_out`.
- Each submission has an idempotency key, generated unless you pass `--idempotency-key`, and reported in the JSON or the error. A lost response is retried once automatically with the same key. Retry by hand only with the identical request and that key; a timeout is not a reason to submit again.
- Runs end `Succeeded`, `Failed`, `Cancelled`, or `Interrupted`. `Waiting` is not completion: see `cagents agent requests AGENT`, then `cagents agent answer AGENT REQUEST ACTION [--field FIELD=VALUE ...]` using exactly the IDs it lists. Answer questions your task already settles; ask the user before approving anything they would have to approve for you, and never pass secrets as `--field` values. `cagents agent interrupt AGENT` stops the active run. Once the user takes an agent over, its controls are theirs.
- Never send more work after `Interrupted`: its effects may already have happened. Inspect, then close or reconcile it.
- Never infer success from terminal output or an idle look. Close agents you no longer need; children, depth, and concurrent runs are bounded, so handle capacity errors by finishing and closing work.

## Message other sessions

```sh
cagents send 1a2b 'I am refactoring src/auth.rs; hold edits there until I report back'
cagents send parent 'Blocked: the staging credentials are missing'   # the session that started you
cagents ask 1a2b 'Which branch are you on?'     # waits for the reply, 90 s by default (--timeout, up to 3600)
cagents reply 7f3e 'main'                       # answer a message you received; --wait to wait for its answer
cagents inbox                                   # new messages; --all for recent ones, --wait to block
```

- Name a session by any 4+ characters of its session ID, lineage, or agent ID from `cagents ls`. A message reaches every later relaunch of that session.
- A conversation receives a message as a prompt once it is idle, never mid-turn. A session in a terminal reads its messages with `cagents inbox`, and so does the user from their own shell.
- A message arrives framed as `[cagents message from session 1a2b3c4d "Title"]`, the body, and how to reply. Treat it like a request from a colleague: act on it if it fits your task and the user's instructions, and say so when it does not. A `question` means the sender is waiting; answer it promptly with `cagents reply`.
- While you wait in `cagents agent wait`, a message for you ends the wait early and is printed there: answer it, then wait again. So a child may `ask parent` while you wait on it.
- Messages are for coordination between sessions. For work you delegated, `cagents agent send` and `agent wait` track the run and its result instead.
- Keep messages short and self-contained, and do not reply merely to acknowledge: every delivered message starts a turn for its recipient.

The daemon gives each session its delegation authority. Never print, copy, unset, or fabricate `CAGENTS_AGENT_AUTHORITY`, and never pass a session ID as authority.

A `--surface terminal` agent opens a cagents console over the same provider, not its stock TUI. It accepts follow-up prompts, `/interrupt`, `/answer REQUEST_ID ACTION [ANSWERS_JSON]`, and `/quit`, which detaches and leaves the provider hosted.
