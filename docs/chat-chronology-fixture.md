# Offline failure chronology validation

Issue #2014 concerns an earlier provider capacity failure appearing after later
user and assistant content when a conversation resumes. The intended native
flow is: read the failure, send a follow-up, watch recovery stream, and continue
reading in conversation order. A later failure with identical wording is a
separate incident. Both failures and their original diagnostics must remain
available in their original positions, including after saved-history reload.
Both fixtures retain the reported first line exactly: `Selected model is at
capacity. Please try a different model.`

`tests/fixtures/chat-chronology-claude.py` is an offline Claude SDK executable.
It needs Python 3, makes no network requests, and never executes the displayed
Bash command. It writes synthetic provider history through the same JSONL path
as the existing [composer fixture](chat-composer-fixture.md). The script emits:

1. A capacity failure in turn 1, with duplicate deliveries of its exact
   assistant carrier and result.
2. Paced assistant text, a streamed tool input, the tool result, and follow-up
   text in turn 2.
3. The byte-identical capacity diagnostic in turn 3, with a new carrier and
   message identity, again delivered twice.
4. Another streamed recovery in turn 4. Further turns provide more recovery
   content for scrolling checks.

Saved history contains one assistant carrier per incident and the completed
text/tool records. Reopening exercises the provider's actual history adapter;
it does not fabricate app snapshots or edit the app database. No timestamps
are needed to decide the fixture's event order.

## Run in an owned native development session

Use the supported development runner in an owned isolated session. Retain any
inherited `CAGENTS_AGENT_AUTHORITY` unchanged; do not remove, replace, copy into
another registry, or impersonate it. Neither the runner nor ordinary native
window startup has a blanket User-shell requirement. Builds and native window
launch must not be held behind a custom fixture approval guard. Coordinate the
runner's build slot with the lead, reuse the configured Cargo cache, and do not
run another development session concurrently. The synthetic provider does not
log in to a real provider account or change the active agent's account.

The CLI setup, account, and cleanup commands below have narrower enforced
boundaries. Scoped requests keep their original capability, and a fresh
isolated daemon does not know the current agent's capability: it returns
`This agent session authority is no longer registered`. The CLI also refuses
daemon lifecycle and account-changing commands under inherited agent authority.
Those operations require a genuine User/operator shell unless an existing
supported path already grants the exact scoped operation. The environment
wrapper below preserves authority and does not make those commands authorized.
Launching a window alone does not establish that fixture setup or the changed
interaction succeeded.

Before switching to a baseline checkout, retain the absolute paths to the
fixtures so both revisions can run the same provider executables:

```sh
fixture_source=$(realpath tests/fixtures/chat-chronology-claude.py)
fixture_codex_source=$(realpath tests/fixtures/chat-chronology-codex.py)
```

From the checkout under test, prepare a fresh ignored directory. Each run needs
its own fresh isolated state and the same synthetic prompts.

```sh
mkdir -p "$PWD/.worktrees"
fixture_root=$(mktemp -d "$PWD/.worktrees/chronology2014.XXXXXX")
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/chron2014.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
ln -s "$fixture_source" "$fixture_root/bin/claude"
git init --initial-branch=main "$fixture_root/project"
fixture_env() {
  env -u CLAUDE_CONFIG_DIR \
    HOME="$fixture_root/home" XDG_RUNTIME_DIR="$fixture_runtime" \
    XDG_DATA_HOME="$fixture_root/data" XDG_CONFIG_HOME="$fixture_root/config" \
    XDG_CACHE_HOME="$fixture_root/cache" PATH="$fixture_root/bin:$PATH" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" "$@"
}
fixture_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui.log"
```

If using the supported nested Niri harness, follow the existing composer
fixture's instructions for absolute `WAYLAND_DISPLAY`, inherited `NIRI_SOCKET`,
and an owned nested session. Keep the short runtime path; do not point these
commands at production or another person's UI or daemon.

Use the newly built matched CLI from the runner's configured target directory.
The project/spawn mutations below require a caller recognized by this isolated
daemon. In a fresh fixture with no registered agent capability, run those
mutations in a genuine operator shell with the same isolated environment:

```sh
fixture_target=$(fixture_env cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
fixture_cli="$fixture_target/dev-runtime/cagents"
fixture_env "$fixture_cli" project open "$fixture_root/project"
fixture_env "$fixture_cli" --json agent spawn --provider claude --model haiku \
  --surface chat --repository "$fixture_root/project" 'Check the saved state.'
```

Record the returned owned agent ID and conversation ID. The provider is the
local synthetic executable, so this uses no paid model. The initial prompt
produces the first failure. Open that chat in the native UI, then:

1. Capture the settled first failure. Expand its diagnostics and verify the
   synthetic request ID and original diagnostic sentence are available. Exact
   duplicate deliveries must produce one visible incident.
2. Send `.`. Observe while the assistant text and tool input are still
   streaming. The first failure must stay above `.` and all recovery output.
   Capture this intermediate state and the completed recovery state.
3. Expand the completed tool activity; select and copy the command/output and
   assistant text. Verify focus, hover actions, and tool disclosure remain
   usable. Keep a text selection while later updates arrive where practical.
4. Send `Try the check again.`. The second failure must appear after that
   prompt. There must now be two incidents, despite identical diagnostics.
   Expand both and confirm neither lost its diagnostic text.
5. Send `Continue.`. Observe the second streamed recovery. The first failure
   must remain before `.`, and the second before `Continue.`. Verify scrolling
   both at the newest content and while reading earlier content; neither old
   incident should jump to the end when activity changes.
6. Repeat at the same narrow width on both revisions. Check no loss of actions,
   diagnostic access, text selection, or scroll position. Additional prompts
   create longer synthetic history if needed; match that workload on both
   revisions.
7. Close only the owned synthetic agent with
   `fixture_env "$fixture_cli" agent close OWNED-AGENT-ID`. Close its isolated
   UI and stop only its isolated daemon with
   `fixture_env "$fixture_cli" daemon stop --require-empty`. If that rejects
   protected sessions, inspect the selected isolated instance; do not force a
   stop or touch production. Relaunch the same fixture runner, use native
   `/resume` to choose the exact saved synthetic conversation, and compare the
   two incident positions and completed tool details. Record that this was a
   fresh history reader, rather than just a reopened resident pane. Continue
   the saved conversation once to verify the next prompt/reply append after
   the restored transcript.

Cleanup stays within the owned fixture: close its synthetic agent and native
session, then use the isolated daemon's supported empty stop. Retain the
fixture history and logs with the evidence until review is complete.

## Codex protocol fixture

`tests/fixtures/chat-chronology-codex.py` exercises the changed Codex protocol
path. It supports app-server stdio and an owned Unix WebSocket, reusing the
existing bounded WebSocket framing from `terminal-escape-codex.py`. Keep both
files in the source checkout when pinning its absolute executable path for a
before/after comparison. This fixture emits two exact `error` deliveries with
`willRetry: false`, followed by duplicate failed `turn/completed` frames. Turn 3
uses a different turn ID with the same diagnostic. Recovery turns emit real
assistant deltas, command output deltas, completed items, and completed turns.
Raw item IDs deliberately repeat across turns, exercising actual turn scoping.

The fixture saves its own synthetic full turn records atomically beneath
`CODEX_HOME` and serves them through `thread/resume` and `thread/turns/list` in
reverse page order. It also writes provider-owned synthetic rollout metadata
for the native `/resume` catalog. These are fixture provider files; no app
database or real provider history is seeded or rewritten. The fixture refuses
to read/write state or bind a socket unless `CODEX_HOME` is private, owned by
the operator, and contains the explicit `fixture-chronology-enabled` marker.
Never put that marker in a real provider account home.

Use a separate fresh root/runtime from the Claude scenario. Preserve authority
and the `fixture_env` isolation above, create this `codex` executable symlink,
and explicitly select its synthetic home. The `fixture_codex_source` path must
still point to the pinned source checkout above, including when the runner
checkout is the before revision:

```sh
mkdir -p "$PWD/.worktrees"
fixture_root=$(mktemp -d "$PWD/.worktrees/chronology2014-codex.XXXXXX")
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/chron2014.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
git init --initial-branch=main "$fixture_root/project"
fixture_codex_home="$fixture_root/home/.codex"
mkdir -m 700 "$fixture_codex_home"
touch "$fixture_codex_home/fixture-chronology-enabled"
ln -s "$fixture_codex_source" "$fixture_root/bin/codex"
fixture_codex_env() {
  fixture_env env CODEX_HOME="$fixture_codex_home" "$@"
}
fixture_codex_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui.log"
```

The active Carson agent identity remains
`codex/4de66f88-ee20-4294-b771-29123ab25166`. This fixture never signs in to that
or any other real provider account. For this user's Codex native run, the only
permitted real account selector is:

```sh
fixture_target=$(fixture_codex_env cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
fixture_cli="$fixture_target/dev-runtime/cagents"
fixture_codex_env "$fixture_cli" project open "$fixture_root/project"
fixture_codex_env "$fixture_cli" --json agent spawn --provider codex \
  --account codex/4de66f88-ee20-4294-b771-29123ab25166 \
  --model gpt-5.4-mini --reasoning-effort low --surface chat \
  --repository "$fixture_root/project" 'Check the saved state.'
```

This spawn command is an unexecuted operator recipe. An isolated development
registry does not automatically contain that managed account. Normal linking
creates a new account ID; it does not assign Carson's existing managed UUID to
a synthetic home. No supported mapping of the exact allowed selector to this
private fixture home has been established, so that Codex native setup remains
missing evidence. A managed account can override inherited `CODEX_HOME`; the
enable-marker check then refuses the wrong home. Do not forge an account UUID,
copy credentials, mark a real account home as synthetic, switch to another real
account, or bypass that refusal. The standalone fixture checks run without any
account or provider session. The synthetic fixture registry and provider files
never establish or override the active Carson identity.

Follow the same prompt sequence and interaction checklist as the Claude
scenario, observing Codex command **output** while it streams. For a fresh
Codex history reader, the account's normal verified shared app-server address
must resolve to an owned instance of this fixture. The operator can launch the
fixture's shared endpoint under the same private runtime, then link only the
synthetic provider home's standard control address to it:

```sh
mkdir -p "$fixture_codex_home/app-server-control"
fixture_codex_env "$fixture_codex_source" app-server \
  --listen "unix://$fixture_runtime/codex.sock" >"$fixture_root/provider.log" 2>&1 &
fixture_shared_pid=$!
ln -s "$fixture_runtime/codex.sock" \
  "$fixture_codex_home/app-server-control/app-server-control.sock"
```

After the four-turn prompt sequence, close only the owned synthetic writer and
its isolated UI, then stop its isolated daemon through the same supported
empty-stop operation:

```sh
fixture_codex_env "$fixture_cli" agent close OWNED-AGENT-ID
fixture_codex_env "$fixture_cli" daemon stop --require-empty
fixture_codex_env scripts/run-development --browserless --debug --log-file "$fixture_root/ui-reopened.log"
```

Keep the shared fixture process alive across this cold-history check. In a
native Codex pane, invoke `/resume`, select the exact saved synthetic thread,
and compare both failures, their diagnostics, and completed command output.
Send `Continue after reopening.` and confirm turn 5 appends after the restored
turn 4. The shared fixture reads the completed provider file on attach; merely
reopening a resident pane does not establish that path. The shared endpoint is
an owned offline provider process, separate from the matched development
daemon. Record the actual endpoint, account mapping, and saved thread ID used
by native attach. These steps have not been executed natively.
Cleanup must address only this owned fixture process/session, through the
operator's normal process ownership controls; do not signal unrelated jobs or
restart production. A stale or absent shared endpoint is a failed history
setup, not evidence that reopening worked.

## Source scope and remaining native setup

The launcher behavior and mutation boundaries are different contracts:

- `scripts/run-development` builds the matched executables and launches the
  normal UI. Native startup calls `ensure_daemon_ready` in
  `src/bin/cagents-ui.rs`; neither entry point rejects inherited agent authority.
  The nested Niri harness also inherits the caller's environment. They can
  launch an owned isolated window without changing that authority.
- `src/control.rs` rejects daemon lifecycle and account-changing CLI commands
  when `CAGENTS_AGENT_AUTHORITY` is present, before daemon startup. Scoped Agent
  requests keep the capability in their business envelope. The isolated
  daemon's `src/host/model/execution.rs::execution_origin` requires that
  capability to be registered in its own execution registry; changing XDG paths
  does not transfer it. Invalid/missing registration has no User fallback.
- Native mutations do not uniformly capture the inherited capability. The
  native chat client and workspace publisher open ordinary UI connections,
  while `src/device_service.rs::request` captures and authenticates the caller's
  authority. Local Accounts UI also has direct registry paths. An agent must
  not automate those User-origin paths to circumvent a scoped setup refusal.

No existing agent-scoped path has been established that provisions the complete
fresh fixture, performs its User-only lifecycle operations, and preserves the
exact allowed Carson account mapping. The concrete missing inputs are a
genuine operator for the User-only fixture setup/cleanup and a supported safe
mapping for that exact Codex account selector. The latter remains unresolved
even though window launch itself has no blanket User-shell restriction. Do
not manufacture either input by transferring authority, editing registries or
databases, or relabeling a synthetic account as Carson. Native window launch
alone, standalone fixture output, and passing source tests do not establish
the changed interaction or its saved-history acceptance.

## Evidence and limits

The fixture sources and their standalone contract tests are preparation, not
native evidence. Run `python3 tests/chat_chronology_fixture.py` to check duplicate
vs distinct identities, provider-history linkage across resume, streaming
frames before aggregate completion, Codex full-history responses, and its
WebSocket handshake. The real adapter/store/projection seam needs its focused
Rust regressions as well.

The operator must record the before revision and exact tested after head,
runner commands, fixture root, generated conversation ID, viewport, scale,
theme, prompt sequence, and whether an actual disk history reload occurred.
Supply actual native screenshots and interaction notes to the independent
reviewer. The reviewer must assess the complete task and inspect streaming,
diagnostic access, duplicate handling, repeated identical failures, selection,
tool disclosure, scrolling, and saved-history behavior.

Capture matched before/after WebP pairs for first recovery, repeated failure,
saved history, and narrow layout. Keep screenshots off the product branch:
commit only images under `pr-screenshots/` on a separate
`<branch>-screenshots` branch. Embed immutable image links in the PR with only
two side-by-side columns, **Before** and **After**, and each state description
below its image pair. Verify all visible content is synthetic.

These fixtures can validate native rendering and interaction through supported
provider stream/history paths when actually executed. Claude evidence covers
the shared projection; it does not validate the changed Codex transport. Codex
evidence must use the Codex fixture. Neither fixture exercises a real capacity
outage, network retry timing, account refresh, OMP transport behavior, or
production-daemon recovery. Synthetic pacing is not a performance benchmark;
compare any responsiveness claims on the same realistic workload. If native
execution or a required state remains unavailable, identify exactly that
missing evidence; do not replace it with a mockup or fixture stdout screenshot.

## In-memory native projection fixture

Development UI builds also accept the exact environment values
`CAGENTS_CHAT_CHRONOLOGY_FIXTURE=capacity-chronology-2014` and
`capacity-chronology-2014-long`. This loads synthetic snapshots into the actual
cagents chat runtime and native window. It uses no provider account, daemon,
project registry, saved session, or authenticated host connection. It does not
change the active Carson account. The inherited authority and account-selection
environment remain untouched. Ordinary builds do not include this mode.

The selected mode is latched before startup. Its app bootstrap, updates,
subscriptions, and close path exclude normal account, device, provider,
persistence, and daemon operations. Local client operations reject before
writing a request or waiting for a reply. The local snapshots remain resident
while the UI changes visibility. Do not add a real provider home or a synthetic
account mapping for this mode.

Use the normal runner with private XDG directories, preserving HOME and any
inherited authority. Retain the coordinated warm Cargo cache and build slot:

```sh
fixture_native_root=$(mktemp -d "$PWD/.worktrees/chronology-native.XXXXXX")
mkdir -m 700 "$fixture_native_root"/{runtime,data,config,cache,metrics}
env XDG_RUNTIME_DIR="$fixture_native_root/runtime" \
  XDG_DATA_HOME="$fixture_native_root/data" \
  XDG_CONFIG_HOME="$fixture_native_root/config" \
  XDG_CACHE_HOME="$fixture_native_root/cache" \
  CAGENTS_CHAT_CHRONOLOGY_FIXTURE=capacity-chronology-2014 \
  CAGENTS_CHAT_STAGE_OUTPUT="$fixture_native_root/metrics/stages.json" \
  scripts/run-development --browserless --debug \
    --log-file "$fixture_native_root/ui.log"
```

Run inside an owned native display. If changing XDG_RUNTIME_DIR beneath an
existing owned Wayland session, preserve its absolute WAYLAND_DISPLAY socket;
otherwise use an owned virtual compositor and nested Niri session. Do not use
another desktop's authentication material. This fixture has no daemon to
restart: the runner still builds the matched executables separately, while the
fixture entry path excludes daemon startup. Device and saved-window options
are rejected. Confirm the visible title says **Offline chronology fixture ·
renderer only** before interacting.

The first capacity error arrives before any assistant response, matching the
reported failure boundary. Expand its Details and send `.` through the real
composer. The fixture clears the draft and publishes paced text and tool
states through the normal refresh path. Then send `Try the check again.` for a
second, byte-identical diagnostic, followed by `Continue.` for another recovery.
Inspect the earlier error during each stream, expand the tool, hover controls,
select text, copy diagnostics and message/tool text, and scroll while updates
arrive. Message Copy uses the complete local synthetic body and the normal
clipboard path; it does not simulate a paged daemon-copy acknowledgement.
Close the owned window normally to flush the opt-in stage report.

The long mode seeds 40 prior exchanges and four additional synthetic chat
cards. Peer snapshots receive 16 bounded local updates. Use the same mode,
input sequence, viewport, scale, theme, sampling interval and instrumentation
for both revisions; report the actual workload and resources. Software-display
measurements do not establish the physical FW16 GPU's performance.

On accepted close, a bounded observer writes one
`cagents_native_fixture_frames=` JSON line to the UI log. It observes actual
redraw-request timestamps without requesting frames or publishing messages.
Duplicate replay timestamps are excluded; an idle transition resets the busy
anchor. Counts and maximum cover the whole run, while percentiles cover at
most the most recent 4,096 busy gaps. This is UI redraw-request cadence, not
GPU presentation, scanout, or input latency. Compare it with CPU/RSS and chat
stage measurements on the same workload and retain the raw observations.

Apply the identical fixture-only harness patch to the pinned before revision
and candidate. Record both tested heads and the harness hash. Native captures
from this mode can validate rendering, dynamic grouping, composer interaction,
diagnostic access, disclosure, selection, clipboard and scrolling. They do not
exercise provider/daemon command acknowledgements, real capacity admission or
recovery, authoritative paged copy, append-delta transport, or provider/Store
history reload. The protocol fixtures and focused Rust regressions retain
those separate scopes. This documentation and the fixture source are
preparation until the native flow has actually run.
