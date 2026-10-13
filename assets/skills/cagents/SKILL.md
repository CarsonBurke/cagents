---
name: cagents
description: Use when working through the cagents CLI, for separate agent sessions, messaging other sessions, or Linear tickets.
---

- `cagents <command> --help` is the reference. Use `--json` when you will parse the output.
- Your own subagents are usually the better choice for work that belongs to this conversation. A cagents session is for another provider, another project, or work the user should be able to watch, take over, or have outlive you.
- `cagents whoami` and `cagents ls` show who else is working in the checkout. Message them rather than editing around them or guessing what they are doing.
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
