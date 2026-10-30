# Deliver a pull request

One delivery owner remains accountable until the requested user outcome is
qualified and delivered. Parallel workers can own implementation regions or
checks; they do not inherit that accountability through a handoff.

## Establish the current work

Read the current PR before changing it:

```sh
gh pr view NUMBER --json headRefOid,headRefName,state,isDraft,body,reviews,statusCheckRollup
gh pr checks NUMBER
```

Name the user task, its bounded implementation scope and the exact revision
being qualified. Compare the description and evidence to that revision; retain
older results only for the source and behavior they actually covered. A missing
GitHub check does not prove that no external job exists: ask its actual owner
for the original job and revision rather than submitting a duplicate.

For repository delivery assignments, prepare the actual worker prompt from a
captured PR and a short task brief before dispatching it:

```sh
gh pr view NUMBER --json number,url,title,state,headRefOid,isDraft > /tmp/delivery-pr.json
scripts/prepare-delivery-task --pr-json /tmp/delivery-pr.json --brief /path/to/task.json
```

Use `scripts/prepare-delivery-task --help` for the brief fields. Consume its
output as the worker's assignment, with only necessary task-specific context.
It checks the captured head and labels an extraction's new working base
separately; it does not fetch fresh state, grant resources or certify readiness.
Recapture changed PR state before dispatch. Keep source-only work independent
of ungranted execution, and require a new exact candidate before qualifying an
extraction. Give current workers explicit steering on their next useful task;
do not restart them merely to refresh instructions.

Before feature fanout, obtain accepted ownership and capacity for independent
review, compilation/tests, affected native interactions and release. A person
may own several stages. A reservation must identify the existing lane or job,
not merely an intended assignee. Respect the shared host's resource budget and
existing leases; an idle process or unlocked path is not permission to use it.

## Execute the next gate

Keep a short delivery entry in the PR: accountable owner, frozen revision,
completed evidence, the next executable command or native interaction and its
owner. Link the actual running job. If it has not started, say queued or
unstarted and name the concrete blocker and resource owner. Update this entry
when the state changes instead of starting another handoff or status loop.

Use the isolated commands and development runner in [development.md](development.md).
Prepare the required inputs before requesting a resource slot, execute the
bounded work when granted, retain original results and release owned resources
before the next owner starts. Missing capacity, a provider limit or an approval
rejection is a real blocker: retain its exact evidence, resolve it through the
supported owner within existing authorization, and do not switch identity or
use an alternate route to bypass it.

When qualification is the blocker, stop enlarging the feature. Resolve the
failed check, missing native fixture, unavailable resource or usability defect
that actually prevents delivery. Do not replace that work with another broad
review campaign. Source review, a prepared controller and a selected test list
are not executed validation.

Freeze the required assertions, workloads, interaction states and affected
feature boundaries. A failed test is not made optional. If its fixture or
oracle is wrong, preserve the original failure and obtain independent causal
review of a focused correction. Keep justified safety and cleanup deadlines;
do not change timers to turn a failure into a pass or add arbitrary periodic
reruns. Do not repeat unchanged reviews without a specific new risk.

Avoid new assembly snapshots or Main joins that add no necessary behavior.
For a required fix, identify what changed and refresh the affected checks and
native evidence. Preserve unaffected results with exact provenance; do not
silently transfer an old full-flow pass to a new integration.

## Review and merge the coherent outcome

Draft means implementation or its design/correctness work is unfinished. When
the coherent implementation is ready for independent review, move it into
review and give outstanding qualification actual ownership and scheduled
capacity. Do not leave finished code unattended in Draft. Ready for review
does not certify merge readiness or relax a required test or native UX gate.

For an interaction change, the reviewer needs the intended task, tested
revision and actual native evidence. The user must retain essential actions,
understand state and recovery, and complete the changed flow at representative
sizes and workloads. Resolve primary-flow failures and evidence gaps before
merge; screenshots, component tests and plans alone are insufficient. Follow
the native comparison requirements in [AGENTS.md](../AGENTS.md).

A smaller extraction is useful only if it completes a coherent task. Inspect
dependencies and preserve access to essential controls before removing a
larger feature's navigation or context. Give the extraction its own executable
qualification and resource ownership; a split is not a waiver for the original
PR's failures. Keep the remaining PR's scope and unfinished work explicit.

Once the required evidence and independent review qualify the outcome, complete
the merge and sync Main. Do not order redundant reviews or unrelated edits as
a substitute for that final delivery action.

## Verify the delivered revision

Record the merged commit, the revision used to build the release artifact and
the actual installed/running revision separately. Installation or deployment
must follow the user's authorization and preserve unsaved work; never restart
a live session solely to fill in a status field. If the installed outcome
cannot yet be verified safely, report it as blocked or unknown rather than
calling the user's problem fixed.

Worker counts and token counters can diagnose resource use. They do not show
that a check ran, that an interaction works or that the fix reached the user.

The bundled cagents skill is embedded in the executable at build time. A source
edit or a newly built CLI alongside an older daemon does not update existing
workers. Qualify its hash/upgrade contract at the matched executor revision;
preserve custom skills and active work. Installing or discovering a skill is
separate from a worker receiving and applying the actual task instructions.
