---
name: cagents
description: Use when working through the cagents CLI, for separate agent sessions, messaging other sessions, Linear tickets, or tracking your channel's PR work.
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

## Delegate toward a delivered outcome

Keep one accountable owner from the user's task through implementation,
independent review, validation, merge and any requested release. Before adding
parallel feature work, agree who can complete its review and required build,
native validation and release work. A task assignment, queued job or idle-looking
lane does not grant execution capacity. Source work can proceed independently
within clear boundaries; builds and native runs use the actual shared allocation.

Give each worker the intended user task, current source revision, surrounding
flow, predicted before/after behavior, file ownership, dependencies and next
delivery gate. Read current PR comments and coordinate overlapping ownership.
Prefer useful independent work over repeated reviews of unchanged heads. Reuse
completed workers where useful; inspect uncertain submissions before resubmitting.
Reconcile stale or superseded assignments without interrupting useful work blindly.

Workers own product judgment within their boundary. Assess whether the assembled
flow lets the person finish the task, rather than treating named edits as the
whole acceptance criterion. For interactions, consider neighboring controls,
hierarchy, spacing, hit targets, selection/hover, truncation, keyboard and narrow
layouts where affected. Explain meaningful design choices and uncertainties to
the lead. A specialist's advice complements this responsibility. Scope the work
to the user's outcome without expanding into an unrelated redesign.

Freeze the coherent candidate before qualification. If verification stalls,
resolve that gate instead of adding features or repeatedly rebuilding an
assembly. Preserve original failures and distinguish source review, compilation,
tests, actual native UX, merge and installed behavior. A compiler failure leaves
tests unrun; a subtask or component pass does not establish the assembled flow.
Give independent reviewers the intended flow, tested revision and actual native
evidence for interactions. Refresh only evidence affected by later changes.
Report the next action and accountable owner until the requested outcome is
delivered. Worker counts and token use describe capacity, not completed outcomes.

## Track the work of your channel

When your authorized task involves a PR, keep the channel's relationship to it
accurate. Use `cagents channel work context --json` to read your authenticated
session's actual channel, then `cagents channel work show --json` to inspect its
existing relations. A checkout, branch name or project membership alone does
not establish that you own the work.

Declare the role that matches the task:

```sh
cagents channel work track https://github.com/OWNER/REPOSITORY/pull/123 contribute --save-request /path/to/task/track-request.json --json
```

- `contribute`: you are implementing changes for this PR.
- `steward`: you are responsible for progressing the PR, such as resolving review findings and getting acceptable work merged.
- `review`: you are reviewing without claiming implementation.
- `reference`: the PR is related context.

Use the complete PR URL, including its actual host and repository. Track each
PR you work on separately, and update your role when the task changes. Inspect
the returned `outcome`; a successful CLI exit alone does not confirm a change.
Do not repeat an unchanged declaration on every turn. To end your participation,
use `cagents channel work end PR_URL`, saving that request too if recovery may
be needed. This does not end other agents' participation. A user's explicit
role correction remains authoritative.

An internal worker may share its lead's session identity. In that case, report
your subtask's relationship to the lead instead of replacing its channel role.
A review subtask does not end the lead's ongoing contribution or stewardship.
Do not invent another caller identity to distinguish internal tasks.

The saved request path must be new and task-owned. If submission is uncertain,
use the reported `channel work result` command with its original registry,
channel, incarnation and operation. Missing receipt means unknown. Retry only
the exact saved request with `cagents channel work apply REQUEST_FILE`; do not
run a fresh `track` or `end` to replace an uncertain operation.

Keep contribution and stewardship separate from verified writes or PR state.
After a PR closes or merges, historical work remains; a reopened PR requires
a fresh declaration before earlier work counts as ongoing again. If your
authenticated channel cannot be resolved, report that gap rather than choosing
another channel, fabricating caller identity or switching authority. Local work
without a PR does not justify inventing a PR association.
