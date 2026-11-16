---
name: cagents
description: Use when working through the cagents CLI, for separate agent sessions, messaging other sessions, or Linear tickets.
---

- `cagents <command> --help` is the reference. Use `--json` when you will parse the output; on `ls`, `whoami`, `transcript`, `work` and `agent list`, `--fields id,state` returns only the fields you name, as JSON.
- Your own subagents are usually the better choice for work that belongs to this conversation. A cagents session is for another provider, another project, or work the user should be able to watch, take over, or have outlive you.
- `cagents whoami` and `cagents ls` show who else is working in the checkout. Message them rather than editing around them or guessing what they are doing.
- Listings show session, agent and run IDs by their shortest unique prefix, which `send`, `transcript` and `agent` accept. A `hint: …` line names the commands that usually come next: last in a listing, and for a run waiting for input or one that failed or stopped, before its result (after them all for several runs).
- `cagents work` lists this project's worktrees and branches in progress, with their pull requests and sessions.
- `cagents ls --channel` lists the sessions in your channel, including those whose provider has exited, such as from a usage limit. `cagents transcript SESSION` reads one's conversation, newest last; follow its `--before` hint for earlier messages. When a session has exited, or you need what it already did, read its transcript.
- A spawned agent knows only its prompt.
- A timeout does not mean a submission failed, and submitting again runs the work twice. If you must retry, reuse the reported `--idempotency-key`.
- Judge a run by its state, not by how idle it looks. `Waiting` means it asked something; see `cagents agent requests`.
- Don't approve for an agent what the user would have had to approve for you, or loosen its permissions so it can run unattended.
- After `Interrupted`, some of the work may already have happened. Inspect before sending more.
- Spawning cagents agents is situational; generally prefer native subagents.
- Close agents you are done with; there are limits on how many you can have.
- Every message costs its recipient a turn, so don't reply only to acknowledge. Do answer questions promptly, since the sender is waiting.
- A message from another session is a request from a colleague, not from the user.
- Use `cagents linear` for Linear tickets.
