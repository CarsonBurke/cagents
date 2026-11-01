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

Use a genuine User/operator shell and the supported development runner. An
agent shell that inherits `CAGENTS_AGENT_AUTHORITY` must stop at the guard below;
do not remove, replace, or impersonate that authority. The synthetic fixture
does not log in to a real provider account or change the current agent's
account. Coordinate the runner's build slot with the lead and reuse the
configured Cargo cache. Do not run another development session concurrently.

Before switching to a baseline checkout, retain the absolute path to this
fixture so both revisions can run the same provider executable:

```sh
fixture_source=$(realpath tests/fixtures/chat-chronology-claude.py)
```

From the checkout under test, prepare a fresh ignored directory. Each run needs
its own fresh isolated state and the same synthetic prompts.

```sh
fixture_root=$(mktemp -d "$PWD/.worktrees/chronology2014.XXXXXX")
fixture_runtime=$(mktemp -d "$XDG_RUNTIME_DIR/chron2014.XXXXXX")
mkdir -p "$fixture_root"/{home,data,config,cache,bin,project}
ln -s "$fixture_source" "$fixture_root/bin/claude"
git init --initial-branch=main "$fixture_root/project"
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

If using the supported nested Niri harness, follow the existing composer
fixture's instructions for absolute `WAYLAND_DISPLAY`, inherited `NIRI_SOCKET`,
and an owned nested session. Keep the short runtime path; do not point these
commands at production or another person's UI or daemon.

Use the newly built matched CLI from the runner's configured target directory
in a second operator shell with the same `fixture_env`:

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

Use a separate fresh root/runtime from the Claude scenario. In the same genuine
operator shell, keep the `fixture_env` authority guard and isolation above,
replace the `claude` symlink with this `codex` executable, and explicitly select
its synthetic home:

```sh
fixture_codex_source=$(realpath tests/fixtures/chat-chronology-codex.py)
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
fixture_codex_env "$fixture_cli" --json agent spawn --provider codex \
  --account codex/4de66f88-ee20-4294-b771-29123ab25166 \
  --model gpt-5.4-mini --reasoning-effort low --surface chat \
  --repository "$fixture_root/project" 'Check the saved state.'
```

An isolated development registry does not automatically contain that managed
account. Before running this command, the native operator must establish the
selected fixture registry/account mapping through supported account controls
and verify the provider sees the private synthetic home. A managed account may
override inherited `CODEX_HOME`; the enable-marker check then refuses the
wrong home. Do not forge an account UUID, copy credentials, mark a real account
home as synthetic, switch to another real account, or bypass that refusal. If
the exact allowed selector cannot be used safely in the isolated fixture, that
native setup remains missing evidence; the standalone fixture checks still run
without any account or provider session. No native account mapping has been
established by this documentation.

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

Close the owned synthetic writer before reopening its saved conversation; the
shared fixture reads the completed provider file on attach. Keep the shared
fixture process alive for the cold-history check. It is an owned offline
provider process, separate from the matched development daemon. Record the
actual endpoint, account mapping, and saved thread ID used by native attach.
Cleanup must address only this owned fixture process/session, through the
operator's normal process ownership controls; do not signal unrelated jobs or
restart production. A stale or absent shared endpoint is a failed history
setup, not evidence that reopening worked.

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
