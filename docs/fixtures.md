# Fixtures

Fixtures let you run the real app, its daemon and its CLI against invented
data and offline providers, so you can see, measure and screenshot a change
without a person's workspace or a live model. They are maintained like product
code: when one lacks a state you need, is flaky, leaks processes or refuses a
legitimate use, fix or extend it in your PR, and leave it easier for the next
person.

## Which one to use

| You want to | Use |
| --- | --- |
| Before/after screenshots of the native app | [`scripts/native-evidence`](development.md#native-evidence), or with `--remote fw16` from a cagents session |
| Drive the app by hand for a while, or run several at once | [`scripts/run-development --instance NAME`](development.md#parallel-instances) |
| A chat that answers offline, with tools, subagents, errors or crashes | The synthetic Claude provider, [`tests/fixtures/chat-composer-claude.sh`](chat-composer-fixture.md) |
| GitHub availability, project avatars or a worktree's pull request in the sidebar, offline | The fixture's forge, [`tests/fixtures/fixture-gh.sh`](#an-offline-github), switched on by `home/.config/fixture-forge` |
| A saved Claude conversation damaged by a crash and a huge tool output, to resume | [`tests/fixtures/claude-damaged-history.py`](../tests/fixtures/claude-damaged-history.py)` PROJECT`, run in a session's fixture with `scripts/native-evidence env python3 ...`; it writes the transcript under the fixture HOME's `.claude` |
| A Codex chat that answers offline, with tool rows, subagent errors or terminal escapes | A [Codex peer](#an-offline-codex), installed by `native-evidence start SIDE --codex PEER` |
| Real Codex app-server ownership, saved-history loading or socket behaviour | [An isolated real executable](#a-real-codex-in-the-fixture), selected by `native-evidence start SIDE --codex-executable /absolute/path/to/codex` |
| Test a script's contract without a compositor or Cargo | The stubbed Python suites in `tests/*.py`, such as `tests/native_evidence.py` |

From a terminal or chat that cagents hosts, your shell carries
`CAGENTS_AGENT_AUTHORITY`. Fixtures that need a genuine User shell refuse it
with exit status 77; never unset, strip or forward it to get past that. Run
them on the build host instead: `scripts/native-evidence --remote fw16 ...`
does every step over SSH, whose login carries no authority, and brings the
screenshots back. See
[Native evidence from a cagents session](development.md#native-evidence-from-a-cagents-session).

## How the pieces fit

A native-evidence session is a stack of isolated layers, each owning its own
directories, so nothing reaches the real desktop, daemon or provider config:

1. **Task directory** (`~/.local/state/cev/TASK`): one per comparison. It
   holds both sides' built executables (`target-before`, `target-after`), the
   fixture data, logs and `shots/`.
2. **Fixture** (`TASK/fixture`): the app's HOME and XDG directories, a
   project repository, and `bin/` with the synthetic `claude`, a `gh` that
   fails until [switched on](#an-offline-github), a `curl` that always
   fails, an `xdg-open` that records each link the app opens in
   `state/opened-urls` rather than starting a browser and, for a session
   started with `--codex`, an [offline `codex`](#an-offline-codex), or an
   explicitly selected
   [real executable](#a-real-codex-in-the-fixture). The fixture is mode 0700. `save-seed` keeps it as the data every later start restores,
   so both sides open the same workspace.
3. **Compositor**: KWin's virtual backend, with its own HOME, hosting a nested
   niri session that `niri-harness` drives (`drive observe`, `drive click`,
   `shot`).
4. **App**: the side's `scripts/run-development --browserless`, started from
   an empty environment. Until its window appears, its `cargo` only accepts
   what `build` compiled, so a session never compiles under the native lock.

`stop` ends all of it: the daemon first, then every process whose environment
names the task's runtime directory or fixture HOME, which is the proof that it
belongs to the task.

## An offline GitHub

The app shows GitHub state, including a project's owner avatar in the sidebar,
only once `gh` reports the project's repository as signed in and readable.
The fixture's `gh` is [`tests/fixtures/fixture-gh.sh`](../tests/fixtures/fixture-gh.sh),
the same file on both sides. It refuses every command until
`$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge` exists. That is the
fixture HOME's `.config`, not the app's `XDG_CONFIG_HOME`, which is the
fixture's `config/`, so no app setting can switch it on. The fixture creates
the directory; the forge reads the switch on every call, so a step can turn it
on in a running session. Then it
answers as a gh signed in to github.com as `fixture`, whose repositories have
no pull requests and no review requests:

| The app runs | It answers |
| --- | --- |
| `gh auth status --json hosts`, with or without `--hostname` | a successful `fixture` account for github.com; `{"hosts":{}}` for any other host |
| `gh repo view --json nameWithOwner,url` | the repository of the checkout's first github.com remote, in gh's order (`upstream`, `github`, `origin`, then the rest), or gh's own error when there is none |
| `gh pr list ... --json FIELDS` | `[]`, or the seeded pull requests below; `--author @me` means `fixture` |
| `gh pr view N` | a seeded pull request, or gh's error for an unknown number |
| `gh pr checks N` | gh's `no checks reported` for a seeded pull request, which the app reads as no checks |
| `gh api graphql` | empty pages for the review-request poll, the pull request browser and its search; not found for a pull request's own queries |
| `gh api repos/OWNER/NAME/issues?creator=LOGIN&...` | the creator's most recently updated seeded pull request, with an `Etag` that changes when that row does, and gh's `HTTP 304` failure when `If-None-Match` still matches it |
| `gh api repos/OWNER/NAME/releases` | `[]`; any other REST endpoint is `404 Not Found` |

Anything else, including every write, fails and names the command. A project
shows as connected to GitHub only when its checkout has a github.com remote,
so add one: `git remote add origin https://github.com/OWNER/NAME`. The fixture
sets `GIT_ALLOW_PROTOCOL=file`, so Git never fetches from or pushes to it.

The named `SidebarPullRequests` and `SidebarHistory` GraphQL queries, and the
Work board's `CagentsAuthoredPullRequests`, use seeded rows too; the board's
lists `fixture`'s seeds, newest `updatedAt` first, and moves each check run's
`workflowName` to `checkSuite.workflowRun.workflow.name`, where GraphQL keeps
it. The open query computes the aggregate check state from the seed's
`statusCheckRollup`; history omits checks. This exercises the native sidebar's
actual query and decoder without a GitHub account.

For polling evidence, create `home/.config/fixture-forge-trace`. The forge then
appends epoch time, parent PID and operation label to
`home/.config/fixture-forge-requests.tsv` for every command it answers. GraphQL
labels name the operation and repository, and REST labels the method and path,
so daemon Work polling can be counted separately from sidebar and branch
lookups. No query bodies or variables are recorded.

Like GitHub, the issues probe moves only with a pull request's `updatedAt` and
the fields of the rows it returns (`per_page` of them, newest first). To make
the Work board, sidebar or review-request scan notice a seeded change, such as
new checks, change that seed's `updatedAt` too, or wait for their five-minute
reread.

Whether or not the switch is on, the fixture's `curl` always fails and Git
reaches only local paths, so a step cannot download a file or clone over https;
copy what it needs from the host's disk instead.

Without `--portal`, a link the app opens, such as a Work card's pull request,
goes to the fixture's `xdg-open`. It appends the link to
`$CAGENTS_EVIDENCE_FIXTURE/state/opened-urls` and succeeds, so a click on a
card neither starts the host's browser nor reaches github.com, and a step can
check what was opened.

Avatars never come from the network. Put an
image at `$CAGENTS_EVIDENCE_FIXTURE/avatars/OWNER.image`, PNG, JPEG, GIF or
WebP, at most 1 MiB and 1024×1024 pixels. Each time the app resolves a
repository, the forge copies every seed into the app's avatar cache,
`$XDG_CACHE_HOME/cagents-development-NAME/github-avatars/github.com-OWNER.image`
for the evidence task's named instance (`cagents-development` for an unnamed run)
with the owner in lower case, and gives it a fresh modification time, so the
app reads it instead of downloading. Write the seed first, as below. The app
loads each owner's avatar once, as soon as an open project has both the remote
and the switch, and remembers a missing one until it restarts.

For a worktree's pull request, `home/.config/fixture-forge-prs.json` holds an
array in the shape `gh pr list --json` prints; `PR_VIEW_FIELDS` in
`src/github.rs` names every field the app asks for. Each entry needs at least
`number`, `url`, `title`, `state` (`OPEN`, `CLOSED` or `MERGED`),
`headRefName`, `baseRefName`, `updatedAt`, `headRepositoryOwner.login` and
`headRepository.name`; the forge refuses a file without them, naming it,
instead of letting the app fail to read it. Each repository lists the entries
whose `url` is under `https://github.com/OWNER/NAME/pull/`, filtered by
`--state`, `--head`, `--author` and `--limit`, in file order rather than gh's
newest first. Reading the file needs `jq` on the build host.

A seeded pull request belongs to a worktree only when all of these match it:

- `headRefName` is the worktree's branch: `main` in the fixture's project.
- `headRepositoryOwner.login` and `headRepository.name` are the owner and name
  of the remote the branch pushes to: its `pushRemote`, `remote.pushDefault`,
  its upstream's remote, or else `origin` or the only remote.
- `url` is under the project's repository, as above.

The worktree's row then shows a pull request icon in place of the branch
icon, and the branch's tooltip names the pull request, its state and its
checks. Entries that match no worktree still answer `gh pr list`, but nothing
in the sidebar shows them.

Seeds cannot support evidence of the review flow. The pull request browser
lists none of them, and opening a seeded pull request's review asks for its
conversation, which the forge answers as not found. Like any failed GitHub
read, that pauses the app's GitHub refreshes for a while.

A steps file for a sidebar with an avatar and the fixture worktree's pull
request. The avatar step needs ImageMagick's `magick` on the build host. It
turns the switch on before the
project gets its github.com remote, since a read that fails while the remote
is there makes the app wait 30 seconds or more before the next. `{fixture}`
is replaced only on the `--remote` route, in forwarded commands and `pair`
steps; typing these commands into `scripts/native-evidence` on the host
itself, write `"$CAGENTS_EVIDENCE_FIXTURE"` inside `env sh -c` instead.

```sh
evidence env sh -c 'touch "$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge"'
evidence env sh -c 'magick -size 64x64 gradient:"#2f6feb"-"#a371f7" "png:$CAGENTS_EVIDENCE_FIXTURE/avatars/octo-org.image"'
prs='[{"number": 12, "title": "Teach the sidebar about avatars", "state": "OPEN",
  "isDraft": false, "headRefName": "main", "baseRefName": "trunk",
  "headRepositoryOwner": {"login": "octo-org"}, "headRepository": {"name": "fixture-app"},
  "author": {"login": "fixture"}, "updatedAt": "2026-11-02T00:00:00Z",
  "url": "https://github.com/octo-org/fixture-app/pull/12"}]'
evidence env sh -c 'printf "%s\n" "$1" >"$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge-prs.json"' sh "$prs"
evidence env git -C '{fixture}/project' remote add origin https://github.com/octo-org/fixture-app
evidence cli project open '{fixture}/project'
# The app reads GitHub in the background; give it time before the shot.
sleep 12
evidence shot sidebar-avatar
```

## An offline Codex

A session started with `--codex PEER`, or a `pair` given it, has a `codex` on
the fixture's PATH that runs one of the offline app-server peers in
`tests/fixtures`. A plain start installs no Codex; a real executable requires
the separate option below.

| PEER | Script | Each prompt |
| --- | --- | --- |
| `tool-rows` | `codex-tool-rows.py` | plays one turn of MCP, dynamic and function tools, collaboration calls, a hook prompt, image generation, a compaction and a diff, started then completed |
| `child-errors` | `codex-child-errors.py` | reports subagent diagnostics before a failed turn; the prompts `failed-turn`, `retry`, `disconnect`, `closed-after-finish` and `auth-required` pick the neighbouring cases |
| `terminal-escape` | `terminal-escape-codex.py` | streams terminal escapes; see the script for the file that completes the stream |

`start` copies the three scripts into `TASK/fixture/libexec/codex`, since the
peers load their transport from beside them, creates `home/.codex`, and
installs [`tests/fixtures/fixture-codex.sh`](../tests/fixtures/fixture-codex.sh)
as `bin/codex`. That passes the fixture as the peer's private root and
`$HOME/.codex` as `CODEX_HOME` unless the app names another, and every peer
checks that both are private to you. Each start installs the peer afresh, so a
seed doesn't carry one into a session that didn't ask for it.

```sh
scripts/native-evidence --remote fw16 --task ~/.local/state/cev/pr1234-rows \
    pair --before origin/main --after . --steps rows.sh --codex tool-rows
```

The peers model the app-server protocol only: no provider, account, model or
network is contacted. The `tool-rows` peer also answers the app's account
check, `codex app-server` over stdio: with an `account/rateLimits/read`
result in `home/.codex/fixture-rate-limits.json`, such as
[`fixtures/accounts/codex-rate-limits-read.json`](../fixtures/accounts/codex-rate-limits-read.json)'s
`result`, the account shows those figures; without one, it reports no usage.
The other peers leave the app's account and usage-limit handling unexercised,
and no peer verifies live account credentials.

For native sign-in controls, prompt the `child-errors` peer with `auth-required`.
It refuses `turn/start` with an explicit authentication error. Put a whole number
of seconds in `home/.codex/fixture-sign-in-seconds` to keep the fake `codex login`
terminal open while **Signing in…** is visible; `login status` stays immediate.
This exercises the app's recovery controls and login-terminal lifecycle without
authenticating an account or contacting a provider. It does not verify provider
credentials or account changes.

## A real Codex in the fixture

Use a real executable when the test depends on Codex's history reader,
thread writer lock or app-server socket behaviour:

```sh
scripts/native-evidence --remote fw16 --task ~/.local/state/cev/pr1234-codex \
    start after --codex-executable /absolute/path/on/fw16/to/codex
```

`--codex-executable` and `--codex` are mutually exclusive. The selected path
must name a readable regular executable ELF on the execution host. Before
waiting for the native lock, the runner reads and snapshots its bytes without
executing it. A queued session keeps that snapshot if the original path is
replaced. Normal fixture preparation installs a fresh copy as
`TASK/fixture/libexec/codex/codex`, with a `bin/codex` link pointing to it.
The copy is mode 0755 and inherits no set-id bits, extended attributes or
capabilities. `TASK/session/codex-executable-installed.json` records the
source path, source/snapshot/copy identities and SHA256 hashes; retain that
receipt with each case's evidence. Each start installs only what it asks for,
so a later plain or fake-peer session cannot inherit the real executable.

Only the executable is copied. The app and provider use the fixture's private
HOME and XDG directories, starting from the runner's empty environment; no
provider config or credentials are imported. For offline history tests, seed
invented data under `TASK/fixture/home/.codex`, configure an offline model
provider there, and keep the rollout's `session_meta.model_provider` equal to
that provider. Use
[`codex-resume-history.py`](../tests/fixtures/codex-resume-history.py) for
turn-framed history and
[`tests/codex_resume_history.py`](../tests/codex_resume_history.py) for a real
app-server example that sends no model prompt. Save the stopped fixture with
`save-seed` before the comparison. This isolates state and credentials; it
does not sandbox the real executable's network access.

## Adding a provider behaviour

The synthetic Claude provider reads switches from files in its config
directory, `$CAGENTS_EVIDENCE_FIXTURE/home/.claude` in a native-evidence
fixture, because the daemon removes `CAGENTS_*` variables from a provider's
environment. To add one:

1. Pick a file name `fixture-WHAT`. A file that exists switches the behaviour
   on; a file whose contents matter (a reply, a delay, frames) is read on every
   turn, so replacing it changes the next turn. A behaviour that should happen
   once, such as a crash, records that it has, as a `"_once": true` frames
   line does in `fixture-frames.exited`, so the resumed provider behaves
   normally; removing the record arms it again.
2. Add it as a function beside the others in
   `tests/fixtures/chat-composer-claude.sh` and call it from the turn loop.
   Emit what Claude Code emits, and record assistant and user frames in the
   synthetic history with `emit_record`, so a reopened chat shows the same
   transcript.
3. Before inventing a frame shape, find the parser that consumes it in `src/`
   and copy the shape the real provider sends. A fixture that emits frames the
   real provider never sends tests nothing.
4. Document it in [chat-composer-fixture.md](chat-composer-fixture.md): the
   file, what each turn does with it, and what the app must show.
5. Test it by running the script directly with crafted standard input and
   checking its output frames, and check the app's rendering through
   native-evidence.

For anything beyond a few frames, `fixture-frames.jsonl` replays an exact
stream with no code change: one frame per line, `@TURN@` for unique IDs,
`{"_sleep": SECONDS}` to hold a running state, `{"_exit": STATUS}` to crash
mid-turn, and `{"_end": true}` to end the turn on the frames' own result. A
`fixture-slow-start` file holds a conversation's provider at start-up. See
[the synthetic provider](chat-composer-fixture.md) for each.

## Reasoning and step rows

[`tests/fixtures/reasoning-row-scene.sh`](../tests/fixtures/reasoning-row-scene.sh)
loads an offline Claude conversation into the actual native app. Its reasoning
is interleaved with three successful commands, a mixed Read/Search/Edit/MCP
group, a failed command and a group with an acknowledged background command.
It includes finished empty reasoning, active empty reasoning and a live thought
that gains a new last line. The Edit input maps to the app's Changes item;
its patch is synthetic. None of the described commands or tools executes.

Run `wide`, `narrow`, then `web` in the same 1200x900 session on each side:

```sh
export CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/my-step-rows
scripts/native-evidence start before --width 1200 --height 900 --limit 900 \
    --ui-trace --codex tool-rows
tests/fixtures/reasoning-row-scene.sh wide
tests/fixtures/reasoning-row-scene.sh narrow
tests/fixtures/reasoning-row-scene.sh web
scripts/native-evidence stop
```

`wide` captures empty active, expanded and updated live reasoning, followed by
expanded mixed tools and failed/running commands. `narrow` reuses that
conversation, adds an unsent blank thread with Ctrl+T, focuses its native tabs,
and uses Alt+R to select one-third width preferences (three cycles on the
hosted chat, one on the draft). The panes share the docked workspace at roughly
270px each. It captures reasoning and the same tools/outcomes at that width.
The blank draft sends no prompt and starts no provider.

`web` stops the fixture Claude actor, closes its unsent draft, and starts the offline Codex peer's opt-in `fixture-web-rows` mode. This sends
a dedicated app-server `webSearch` item with two synthetic results; Claude's
WebSearch tool uses the generic Tool representation and cannot cover this row.
Every subsequent actor explicitly uses the initial fixture channel, avoiding
another sidebar hierarchy when a CLI call has no selected view. It captures the group collapsed and its nested result detail expanded at the
same narrow width. The peer does not launch a child or publish unrelated tool
or diff rows in this mode. Result links use `example.test`; no search is sent.

Keep the native window at 1200x900: below the docked inspector's minimum
workspace width it becomes an overlay that hides the transcript. Run each
mode once, in order. For a remote `pair`, its steps file contains:

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" wide
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" narrow
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" web
```

For the neighboring disclosure controls, continue with `coordination` then
`diagnostics` in that same session. The first atomically stages
[`coordination.jsonl`](../tests/fixtures/reasoning-row-scene/coordination.jsonl)
and starts another offline Claude conversation. It captures folded cards, the
full Agent prompt, raw tool call, full SendMessage body and delivered report.
The report comes from a completed task notification; the child transcript is
also populated by a live child frame. Exercise the raw Input block's hover
Copy action and the footer's Transcript link during live review. The report's
Copy action is checked against its final `REPORT_END` paragraph where that
control exists. The diagnostic mode switches the offline Codex peer to
`fixture-row-diagnostics`, emitting an original root error followed by a failed
turn. It captures Details closed/open and checks Copy details through the
nested compositor's private clipboard. Neither mode reaches the host clipboard.

All provider settings and transcripts stay inside the task's private fixture;
no live model, provider account or real provider home is used. Stop afterward:
the final thought deliberately stays active for 600 seconds. The background
command is a recorded protocol acknowledgement, not a sleeping process.
`--ui-trace` retains UI-thread summaries and calls over 50ms. Compare these and
bounded CPU/RSS samples during the same captures, and exercise hover/scroll
while the latest-line delta arrives. This short fixture has one working
conversation, a blank draft and later a second synthetic conversation; it does
not establish performance for long histories or many concurrent agents.

## Adding a scene to evidence

Write the scene as steps, not as instructions in a PR comment, so the next
agent can rerun it:

```sh
evidence cli project open '{fixture}/project'
evidence env sh -c 'printf "%s\n" "# Findings" "The cache key ignores the locale." \
    >"$CAGENTS_EVIDENCE_FIXTURE/home/.claude/fixture-reply.md"'
evidence drive click 'Open repository'
evidence shot channel-prs
```

`scripts/native-evidence --remote fw16 --task DIR pair --before origin/main
--after . --steps FILE` runs such a file on both sides; options such as
`--width 1200` or `--portal gtk`, for a scene that attaches a file through the
real file chooser, go to each side's `start`. Prepare data that both
sides need once, then `save-seed` it, rather than repeating slow setup in each
side's steps. `{fixture}` in a steps file means the fixture's root on the
host. `pair` can wait 30 minutes for a native slot, so run it in the
background rather than under a short command timeout, and run `clean` once the
PR merges to remove the task's checkouts from the host.

## Saved Codex history

For saved Codex turns in the Resume conversation picker, use
[`tests/fixtures/codex-resume-history.py`](../tests/fixtures/codex-resume-history.py).

`/usr/bin/python3 tests/fixtures/codex-resume-history.py ROOT --turns 3`
creates one invented conversation under `ROOT/home/.codex` and prints its
thread ID, rollout path and working directory as JSON. Run it once in a new
fixture before saving the seed; it refuses an existing Codex home. For native
evidence, use that home with a real Codex executable and the ordinary development
runner. The generator does not install or launch Codex, sign in, or call a model.

Each user message has a `task_started` event before it, and each turn ends with
`task_complete`. The rollout also includes the user and agent message events
that Codex consumes for history. Model `response_item` records alone can leave
the native history empty even though the Resume picker finds the conversation.
`/usr/bin/python3 -I -B tests/codex_resume_history.py -v` checks the fixture;
set `CAGENTS_TEST_CODEX` to an absolute native Codex executable to also verify
`thread/resume` and `thread/turns/list` without starting a model turn. That
optional check gives Codex fresh HOME, config and cache directories and an
unreachable local model endpoint. It compares the original incomplete fixture
shape with the corrected one, including message contents and turn identities.

## When a fixture fails

Every refusal names its cause and the next command. The common ones:

| Message | Cause and fix |
| --- | --- |
| `requires a genuine User shell` (exit 77) | You are in a cagents session. Add `--remote fw16`. |
| `task directory must be outside Git ancestry` | Recovery storage refuses to live in a checkout. Use `~/.local/state/cev/NAME`. |
| `changed since its build; run: build SIDE CHECKOUT` | Evidence must show what was built. Build again. |
| `has not been built, or its unused build was reclaimed` | `scripts/prune-build-dirs` removed an idle target. Build again; a shared slot recompiles only what changed. |
| `refusing to compile ... while holding the native lock` | Something in the session asked Cargo for a build that `build` didn't do. Build first; keep builds out of a session. |
| `every native slot stayed busy` | Other sessions hold every slot. The message names the slot files; `scripts/lock-holders` given them shows whose. |
| `a session may hold a native slot for at most 1800 seconds` | `--limit` asked for longer. Capture in one short session, or split a long measurement across sessions. |
| `memory-capped: stopped` | The session exceeded its memory limit; the log names the process. |
| `session: lost` in `status` | Its supervisor was killed. `stop` ends what remains. |
| `the SIDE side is running on HOST; stop it before rebuilding` | A side's checkout can't change under its session. Run the `stop` it names, then build. |
| `task NAME on HOST belongs to OWNER` | Another agent's task has that name on the host. Give your task directory another name, such as your PR's. |
| `... is not a task this route started` | A task of that name exists on the host without an owner: someone runs it there directly, or an earlier version of the route started it. If it is yours, run the command the message gives on the host; otherwise rename yours. |
| `codex: the fixture has no Codex peer` | The session started without `--codex`, but something ran `codex`. Start it with `--codex tool-rows`, `child-errors` or `terminal-escape`. |
| `fixture requires private owned root/runtime directories` | A Codex peer found the fixture or runtime directory readable by others. `start` makes both private; if you changed their modes, start again. |
| `this task directory is the host's own` | You are on the build host. Run `scripts/native-evidence` without `--remote`. |

If a fixture fails in a way that doesn't point to the fix, that is a fixture
bug: make the message say what is wrong and what to do, and add a test for it
beside the others in the fixture's contract suite.

## A read-only native update review

Development UI builds under `native-evidence` read
`home/.config/fixture-update-review.json` in `CAGENTS_EVIDENCE_FIXTURE`.
Copy `tests/fixtures/update-review.json` there with mode 0600, then start the
session (or wait for the active review's next status poll). The example includes
running work, a blocked conversation, a daemon notice and a paused terminal.
The file must contain an awaiting-confirmation job named `fixture-update-review`
with a blocked daemon preview. Apply is disabled; decisions fail before looking
up an installation. Remove the file to end the fixture review.

This loads data into the actual native dialog and supports scrolling and Copy
details. It does not run providers, inspect a live update inventory, perform an
update, or exercise cancellation/recovery. The grouped Paused heading belongs
to the CLI review. The native dialog currently omits saved stopped panes, so
the fixture's paused terminal does not appear in that dialog.
