# Current agent status fixture

This is invented, labeled transcript data using Claude Code frame shapes already
recorded in `claude-waiting-agent-transcript.jsonl`,
`claude-stop-waiting-agent.jsonl`, and `message-card-scene/frames.jsonl`.
It contains no user conversation, credentials, or provider calls.

`live-agent-status-history.jsonl` is a saved transcript. It has twelve async
launch receipts without completion receipts, four completed agents, and a failed
Retry task. `live-agent-status-live.jsonl` resumes Retry with the same provider
agent ID through a new Agent tool call and completes it. It then starts three
workers, a Planner paused on a rate limit, a Soak agent waiting on background
work, a fresh failed rollback check, and an agent explicitly killed by its lead.
Worker 1 launches a nested helper in its own transcript.

`live-agent-status-multiple.jsonl` is a follow-up turn that retries rollback
with the same provider agent ID and a new tool call, then reports a fresh
failure. It adds two more failed agents with the same unnamed
`general-purpose` kind and different task descriptions. One task description
is long enough to exercise clipping in a narrow view. These three fresh
failures are current after the actual user prompt retired the preceding turn's
failure. None fabricates child prose: the diagnostic comes only from the
realistic task-notification result.

`live-agent-status-long.jsonl` then resumes and completes the rollback and
signature checks. It resumes the recovery audit with its same provider ID and
a new tool call, and reports a fresh failure in this turn. Only that long
recovery task is currently failed, so the status row itself must fade its full
single-failure description within one line.

After loading history, no agent has evidence of current work. After the live
phase, the direct conversation depends on three working agents, one paused
agent, and one agent waiting on background work. Only **Check installer rollback**
is a current failure. Retry's earlier failure is superseded, the killed agent
is stopped, and the nested helper belongs to Worker 1's task. Planner's recorded
rate-limit frame tests a general waiting state, not an actual question to the
reader.

The history file uses persisted `toolUseResult`; live frames use stream-json
`tool_use_result`. The synthetic provider's `_live` directives keep forwarded
child frames out of the main history. `_end` ends the replay without appending
an unrelated provider answer. Tests feeding the adapter directly should strip
these fixture directives.

Run `live-agent-status-scene.sh` in a ready native-evidence session on fw16.
It seeds only that fixture's private HOME and opens the saved conversation
through the native Resume picker before sending the live prompt. A repeated
seed refuses to overwrite the existing transcript. Native-evidence starts each
side from a fresh fixture unless a seed was explicitly saved.

The scene detects whether the installed harness calls pointer motion `move`
or `hover`. Failure controls and chooser tasks are located through their visible
captions, so the scene also supports older OCR-only harnesses.

The scene reads its JSONL and seed script from beside itself. Set
`CAGENTS_EVIDENCE_COMMAND` to an executable `native-evidence` path to run a
prepared copy of the scene outside the repository tool checkout. The default
is `../../scripts/native-evidence` relative to the scene. For example:

```sh
CAGENTS_EVIDENCE_COMMAND="$tool/scripts/native-evidence" \
CAGENTS_EVIDENCE_TASK="$task" "$task/scene/live-agent-status-scene.sh"
```

A remote pair steps file can contain:

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/live-agent-status-scene.sh"
```

Run `native-evidence pair` with `--width 1200 --height 900` and set
`CAGENTS_EVIDENCE_NARROW_WIDTH=900` in the steps file when invoking the scene.
For each ordinary screenshot, the scene captures the full-width state, resizes
only the task's private native column to 900 logical pixels, captures the
`narrow-` state, and restores the column to 100% of its viewport. This captures
both layouts in a single session per revision. The framebuffer stays wide;
`layout-SIDE-narrow-STATE.json` in the task directory records the observed
native window size for each narrow shot. Hover tooltips are captured at
the initial wide width. All compositor commands run through `native-evidence
nested`; the host desktop is never addressed. Alternatively, repeat the pair
with `--width 900 --height 900` and pass `narrow-` as the scene's argument.
The scene captures loaded history, current status, failure hover,
and the result of clicking the failure, including expansion of its notification
diagnostic. The follow-up captures three failures, their tooltip, the Failed
chooser with task descriptions, and selection and expanded diagnostics of the
long recovery task. A final shot leaves that long task as the only failure
to exercise the status row's horizontal fade. The shared agent name must not
make the tasks ambiguous. On Before, hovering and clicking the
passive status row shows no failure details and leaves the conversation selected;
on After, the failure control identifies the task and selects its transcript.
The comparison exercises the native UI and Claude adapter against offline
frames. It does not establish live Claude process liveness or provider behavior.
