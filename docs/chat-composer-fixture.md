# Offline chat composer validation

For how fixtures fit together and how to add a behaviour, see [Fixtures](fixtures.md).

`tests/fixtures/chat-composer-claude.sh` is a fake Claude stream-json executable
for native UI checks. It requires `jq`, makes no network requests, and uses a
synthetic account. It advertises asynchronous message support, holds its first
turn busy, acknowledges queued-message removal, and settles on interrupt.
Later turns complete immediately. Busy messages deliberately stay queued;
this fixture checks composer interactions, not provider delivery.

From a shell without a desktop session, such as plain SSH,
[`scripts/native-evidence`](development.md#native-evidence) sets up this environment;
you still open the project and start the conversation. From a session cagents
hosts, add `--remote fw16` to run it on the build host
([Native evidence from a cagents session](development.md#native-evidence-from-a-cagents-session)).
By hand, use a new directory beneath the repository's ignored `.worktrees/` for every
run. Keep its HOME, XDG data/config/cache, daemon runtime, and project isolated
from the development application you normally use. For example, from the
checkout being validated:

```sh
fixture_root="$PWD/.worktrees/chat-composer-fixture"
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/composer.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
ln -s "$PWD/tests/fixtures/chat-composer-claude.sh" "$fixture_root/bin/claude"
git init --initial-branch=main "$fixture_root/project"
```

Run the UI through `scripts/run-development --browserless`. Prefix both the
runner and every CLI command with the same fixture environment:

```sh
fixture_env() {
  if [ "${CAGENTS_AGENT_AUTHORITY+x}" ]; then
    printf '%s\n' 'This fixture requires a genuine User shell; inherited agent authority was not modified.' >&2
    return 77
  fi
  env -u CLAUDE_CONFIG_DIR \
    HOME="$fixture_root/home" XDG_RUNTIME_DIR="$fixture_runtime" \
    XDG_DATA_HOME="$fixture_root/data" XDG_CONFIG_HOME="$fixture_root/config" \
    XDG_CACHE_HOME="$fixture_root/cache" PATH="$fixture_root/bin:$PATH" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" "$@"
}
fixture_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui.log"
```

Keep the existing Cargo target/cache configuration when using a worktree; do
not start another build while the runner builds its matched executables. If the
harness help exposes `session start NAME --root ROOT --cmd CMD`, start a nested
Niri session with `niri-harness session start composer --root "$fixture_root/niri" --no-window-wait --cmd WRAPPER`,
where `WRAPPER` is a shell command that runs the fixture runner script. Before replacing
`XDG_RUNTIME_DIR`, convert the inherited relative `WAYLAND_DISPLAY` to an
absolute path (`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`). Keep the inherited
`NIRI_SOCKET`, and use only this owned nested session for input and screenshots.
The runtime directory needs a short path because Unix sockets have a path-length
limit; keep it under the original XDG runtime directory rather than the checkout.

After the UI starts, use its newly built `cagents` CLI under the fixture
environment to create the project and synthetic agent:

```sh
fixture_cli="${CARGO_TARGET_DIR:-$PWD/target}/dev-runtime/cagents"
fixture_env "$fixture_cli" project open "$fixture_root/project"
fixture_env "$fixture_cli" --json agent spawn --provider claude --model haiku --surface chat \
  --repository "$fixture_root/project" \
  --prompt 'Can you investigate why the Kaggle submission failed?'
```

Open the resulting chat in the UI and check:

1. Enter a message and choose the **Steer** icon; hover its queued timing icon
   to confirm next-step delivery.
2. Enter a long URL and choose the **Queue** icon; hover its timing icon to
   confirm next-turn delivery. Its preview truncates within the available width
   without hiding its actions.
3. Capture the busy composer, including a separate unsent draft.
4. Recall a queued message with the **Edit** pencil; confirm its text moves into the
   composer and its queued entry disappears. Queue it again if needed.
5. Remove every queued entry, then click the **Stop** icon. Confirm the composer returns
   to its idle send state. Clear the draft and capture that state.
6. Repeat the queued-state checks at a narrow window width, including keyboard
   focus and the split composer toolbar.

Store screenshots under `docs/screenshots/` for PR evidence. Stop the owned
nested session, close its synthetic agent, and stop only its isolated daemon
when finished. The fixture directory can then be removed.

The daemon removes `CAGENTS_*` variables from a provider's environment, so
optional fixture behaviors are switched on by files in
`$fixture_root/home/.claude`, the fixture's Claude config directory. The
reply and error files are read for every prompt; the others take effect from
the provider's next start.

To reproduce resume branching, write an earlier main record's UUID to
`fixture-resume-parent`. Each resumed provider then continues from that record,
keeping the existing transcript and its full turn counter. Remove the file to
resume from the last record again. A missing or private record is refused.
This exercises transcript history and reopening; it does not reproduce why a
live Claude Code process chooses an earlier parent.

To check how an answer renders, copy a Markdown file to `fixture-reply.md`.
Each later turn answers with that file's contents instead of the default
sentence, so replacing the file changes the next answer. A few turns with a
long file give a transcript long enough to scroll.

Create `fixture-stream-reply` to send that answer in text deltas before its
final record. Its first line may hold the delay between deltas in seconds
(default `0.25`), and its second line the characters per delta (default
`16384`). `0.03` and `24` approximate a live provider's streaming rate, so
several fixture agents give a realistic concurrent load. While streaming,
control requests still take effect and additional prompts receive the usual
queued lifecycle. Interrupt stops the stream and settles the turn without
recording an answer that was never fully sent. Closing stdin finishes the
finite reply. The fixture requires Bash for timed input reads; `python3
tests/chat_composer_streaming.py` checks this duplex protocol without the app
or a live provider.

For provider failure checks, write a diagnostic such as `Failed to
authenticate: OAuth session expired and could not be refreshed` to
`fixture-image-prompt-error`. Any prompt containing an image then settles with
that error before any reply. One Send of text plus images must show one failure
notice.

To check where a failure is drawn after delivered work, write a diagnostic to
`fixture-delivery-error` instead. Every prompt then gets a reply, a background
command's completion notice, a follow-up, and that error. The notice must
appear once, after the follow-up.

To check recovery after the provider dies mid-turn, as when the system runs
out of memory, create `fixture-stop-on-prompt`. The next prompt gets the start
of an answer, then the provider kills itself with SIGKILL; the file is removed
first, so a resumed provider answers normally. Create an empty
`fixture-launches` as well to record each provider start: a line holds the
conversation ID it resumed, or `new`. After pressing **Resume conversation**
on the stopped card, the file ends with exactly one line naming the
conversation that stopped, and no prompt is sent until you send one.
To see a resume that fails, create `fixture-fail-resume` too: the next
resumed start exits before it answers, and the card offers **Try again**. The
file is removed first, so pressing it resumes normally.

A resumed transcript may be damaged, as a crash leaves one. The provider
skips each line that isn't a JSON record, as Claude Code does. It then
goes on from the last record it could read, a simplification: Claude Code
follows the `parentUuid` chain to its leaf. A last line cut short without a newline
is ended first, so the next record starts its own line.

A prompt of exactly `/compact` compacts the conversation as Claude Code
does: the stream gets a `compact_boundary` and the summary, and the history
gets a boundary that starts a new parent chain, linked back to the earlier
turns only by `logicalParentUuid`, then the summary and the command recorded
as one run in the terminal, with its output. To offer **Compact** in the composer,
advertise the command in `fixture-commands.json`, such as
`{"commands":[{"name":"compact","description":"Compact"}]}`. Reopen the
conversation afterwards to check that the turns before the compaction are
still there, followed by **Context compacted** with the summary inside.

For tool disclosure checks, create `fixture-verbosity`. Each turn emits a
streamed MCP call with a large prompt, a Skill call, a failed MCP call, and a
multiline Bash command. It records complete tool inputs and results in the
synthetic provider history, so the same transcript can be reopened.

In the native chat, expand the activity group and check that the closed MCP
row says `src/app.rs`, the Skill row says `writing-copy`, and neither shows
internal framing. The failed `private.txt` step must retain `Permission denied`.
The shell preview must show `cat <<EOF` without its heredoc payload. Expand
individual steps and verify their complete Input/Output remains selectable
and copyable. Collapse them while the first turn is still busy, stop that
turn, and reopen the saved chat; compare the completed and restored content.
The fixture represents a reproducible payload path, not a captured user
conversation or evidence about the provider's answer prose.

For subagent tab checks, create `fixture-subagents` before spawning the agent
(`touch "$fixture_root/home/.claude/fixture-subagents"`). Each turn starts
five Agent calls before its answer: Scout keeps working in the background,
Planner waits on a rate limit in the background, Builder fails, and two
unnamed Explore agents finish. The tabs beside Conversation must read Scout,
Planner, Builder, Explore and Explore 2, in that order, each with its state's
mark; hovering a tab shows its state and task. A selected agent's Stop ends
just that agent.

Select Builder and focus its tab. The first turn stays busy, so stop it
before sending another prompt: because Stop also ends Scout and Planner, the
first press only asks, and a second press within a few seconds confirms.
The fixture then reports both as stopped. The next prompt starts five more
agents, numbered after the first ones (Scout 2, Planner 2, Builder 2,
Explore 3 and Explore 4), and Builder keeps its number, the selection and the
keyboard focus while the strip re-sorts. The scene covers Claude's Agent
calls only; it does not exercise Codex nicknames.

For a chosen set of tool calls, write `fixture-tools.jsonl`: one call per
line, as `{"name":"Bash","input":{"command":"cargo test","description":"Run
the tests"},"result":"error[E0308]: mismatched types","is_error":true}`.
Every turn emits each call and its result in order, before its answer, and
records them in the synthetic history like the other behaviors.

To replay a provider stream exactly, write `fixture-frames.jsonl`: one
stream-json frame per line, emitted in order on every turn before its answer.
`@TURN@` in any string becomes the turn number, so IDs stay unique across
turns. Assistant and user frames are recorded in the synthetic history; system
frames, such as a background task's `task_started` and `task_notification`,
are live only, as they are from Claude Code. A frame with `"_history": true` is
only recorded, the way Claude Code records a queued notification as a
`queue-operation`. A line `{"_sleep": SECONDS}` pauses the stream, so a running
state can be captured. A line `{"_exit": STATUS}` makes the provider exit
there with that status, from 0 to 255, mid-turn and without a result, as a
crash does. Every turn that reaches the line exits again: the app doesn't
resend the interrupted prompt, but the next prompt sent after it reconnects
replays the same frames. Add `"_once": true` to exit only the first time a
turn reaches that line: the fixture records each such line's number in
`fixture-frames.exited`, so the reconnected provider goes on, and removing
that file arms them all again.

A Stop on an agent the frames started answers as Claude Code 2.1.296 does
for an agent that already sent its notice and is waiting on its own
background work: a `task_updated` frame patching the task `killed`, with no
further notice. `tests/fixtures/waiting-agent-scene.sh` builds a scene on
this: an agent that reports it is waiting, then is resumed, finishes, fails,
is resumed elsewhere, or is stopped from its tab.
`tests/fixtures/message-card-scene.sh` builds another: an agent started in
the background, then messages to it with Claude Code's `SendMessage`, one
short, one long enough to fold, one Claude refuses and one to an agent the
conversation does not list, each answered with the JSON Claude Code returns.

To check model and usage reporting, copy a recorded `initialize` response to
`fixture-models.json`, such as `fixtures/accounts/claude-initialize-models.json`,
the list Claude Code 2.1.296 offered with Fable 5.2. Its `models` replace the
default list, and from the provider's next start each turn's `init` names the
full ID the chosen model resolves to, as Claude Code's does: `opus` runs
`claude-opus-5-5`. The model chosen with `--model` or in the picker is the one
resolved; without either, the conversation starts on the list's `default`
entry, as Claude Code's does. To give the account check figures, write a
`get_usage` control response's inner `response` to `fixture-usage.json`; the
account check and each conversation read it whenever they ask. A live usage change is a
`rate_limit_event` frame in `fixture-frames.jsonl`, such as
`fixtures/accounts/claude-rate-limit-event-fable.json`, which a Fable turn
reported.

For multiple synthetic accounts, put an `auth status --json` response in each
account's `fixture-auth-status.json`, such as
`{"loggedIn":true,"email":"account-one@example.test","subscriptionType":"pro"}`.
The fixture reads it from `CLAUDE_CONFIG_DIR`, or `HOME/.claude` when no config
directory is selected. Give each account a distinct email so the app treats
the homes as separate sign-ins, and put its usage response in that home's
`fixture-usage.json`. Without an auth file, the fixture keeps its default
`fixture@example.test` identity. Keep all these homes inside the isolated fixture;
never write them into a real provider home.

To hold a conversation's provider at start-up, as a cold start does, create
`home/.claude/fixture-slow-start`. The provider then waits before it reads
anything: the number of seconds on the file's first line, such as `1.5`, or
5 if the file is empty. Anything else makes it exit with status 2 and say so. The app's account check is not delayed. Use this to capture the
starting state, or to check what the app does when a prompt is sent before
the provider answers.

To have the first turn finish by itself, as real work does, write a whole
number of seconds, such as `8`, to `home/.claude/fixture-first-turn-seconds`.
The first turn then stays busy that long, unless it is interrupted first, and
ends successfully. Anything else makes the provider exit with status 2 and say so.
Use this to check what happens to prompts and messages that wait behind an
agent's run once that run ends.

To run a real command, write it as the first line of
`home/.claude/fixture-run-command`, such as `sleep 6001`. Every turn then runs
it as a Bash call in a child of the provider, as Claude's Bash tool does, and
waits for it before answering, reporting its output and exit code. Use this to
check where a provider's commands run and what happens when one is stopped:
from `scripts/native-evidence env`, find the command's cgroup in
`/proc/PID/cgroup`, then stop it as systemd-oomd does, by setting
`user.oomd_ooms` on the cgroup and writing `1` to its `cgroup.kill`.

`home/.claude/fixture-run-once` runs its first line the same way, but only in
the next turn to start, which removes it; that turn runs it instead of
`fixture-run-command`. Agents sharing the fixture configuration can then act
one at a time: write `cagents send SESSION 'Ship it'` there just before giving
the sending agent its turn, and only that agent sends it.

For image preview and viewer checks, copy a PNG to
`"$fixture_root/home/fixture-image.png"` before spawning the agent. Native
launches drop `CAGENTS_*` variables, so the fixture reads the image from its
provider home; `CAGENTS_FIXTURE_IMAGE` overrides the path when the script runs
directly. Each turn then also returns that image as a provider-produced base64
image block, recorded in the synthetic history like other frames. Paste an image into the composer to
cover a user-attached image in the same transcript. Use synthetic images only.
