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
| A chat whose inspector lists the pull requests and worktrees it worked on | [`tests/fixtures/conversation-work.py`](#conversation-work), run through `scripts/native-evidence env python3 ...` before starting a Claude chat, or an Antigravity or Muse chat prompted with `work` |
| A pull request review read from cache while GitHub is unavailable, with a restored draft line comment | [`tests/fixtures/offline-review-cache.py`](#an-offline-cached-review), run through `scripts/native-evidence env python3 ...` once the app has started |
| A long saved chat for paging backward and forward | [`tests/fixtures/claude-long-history.py`](../tests/fixtures/claude-long-history.py)` PROJECT`, run through `scripts/native-evidence env python3 ...`; 260 numbered exchanges exceed the retained history window |
| A Codex chat that answers offline, with tool rows, subagent errors or terminal escapes | A [Codex peer](#an-offline-codex), installed by `native-evidence start SIDE --codex PEER` |
| Real Codex app-server ownership, saved-history loading or socket behaviour | [An isolated real executable](#a-real-codex-in-the-fixture), selected by `native-evidence start SIDE --codex-executable /absolute/path/to/codex` |
| A daemon still restoring after an update, or holding its restore for recovery | [The startup recovery marker](#an-update-restore-in-progress-or-held), `home/.cagents-fixture-startup-recovery` |
| A conversation whose restart the update stopped | [The stopped-restart marker](#a-conversation-the-update-stopped), `home/.cagents-fixture-update-stopped` |
| An offline ACP agent shaped like Antigravity or Muse, with tools, approvals, stops, saved sessions and sign-in failures | [`tests/fixtures/fake-acp-agent.py`](#an-offline-acp-agent) |
| The offer, after an update, to resume conversations whose work it stopped | [`tests/fixtures/update-resume-scene.sh`](#an-update-that-stopped-work-in-progress), which records an ended update over four offline agents and reopens the window |
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
   Restoring a seed keeps existing provider homes and transcript files in place,
   including read-only files. Updated files regain the seed's saved permissions;
   fixture symlinks are not followed when preparing files for restoration.
3. **Compositor**: KWin's virtual backend, with its own HOME, hosting a nested
   niri session that `niri-harness` drives (`drive observe`, `drive click`,
   `shot`).
4. **App**: the side's `scripts/run-development --browserless`, started from
   an empty environment. Until its window appears, its `cargo` only accepts
   what `build` compiled, so a session never compiles under the native lock.

`stop` ends all of it: the daemon first, then every process whose environment
names the task's runtime directory or fixture HOME, which is the proof that it
belongs to the task.

## A long paged chat

On the build host, run this in a native-evidence session before opening the
saved conversation:

```sh
scripts/native-evidence env python3 "$PWD/tests/fixtures/claude-long-history.py" \
    "$CAGENTS_EVIDENCE_TASK/fixture/project" --tool-bytes 4194304
scripts/native-evidence cli project open "$CAGENTS_EVIDENCE_TASK/fixture/project"
```

Choose **Resume conversation** from the new-tab menu and open **History paging
checkpoints**. Its 260 numbered exchanges include a 4 MiB Bash result every 32
turns, numbered line by line. Each is one command row whose expanded output
shows its first and last lines, with a line between them saying how much is
left out, and whose Copy takes the whole result. Page backward past several of
them, then wheel downward to check that newer history loads without clicking
its fallback button. Check that the visible checkpoint stays anchored when the
next page arrives. No provider command is executed.

Omit `--tool-bytes` for ordinary completed prose. `--scale 64` increases the
paragraph count enough to exercise provider history paging with prose alone.
`--untitled` leaves out the conversation's name and `--first-prompt TEXT`
replaces the first checkpoint, for a channel that only its first prompt can
name, which lies outside the loaded window.
`--working 16` gives each exchange 16 commands, each with the hook records
Claude Code writes around a tool call, and a one-line answer. A saved page of
100 records then holds about two exchanges, as in an agent's long working
session, so opening the conversation shows whether the transcript fills its
window from further pages without the reader asking.
The generator writes under `HOME/.claude`; always run it through
`native-evidence env`, which supplies the private fixture HOME. Stop and
`save-seed` after setup to reuse exactly the same conversation on both sides.

## Conversation work

A chat pane's inspector lists the work its conversation did: the pull
requests it created, pushed, edited or updated, the worktrees it edited, and
the pull requests it only mentioned. The daemon reads these from the
provider's own transcript and asks the forge about each.
[`tests/fixtures/conversation-work.py`](../tests/fixtures/conversation-work.py)
prepares a fixture so that the next Claude chat there has all of them. Run it
in a session before starting the chat; it takes no arguments:

```sh
scripts/native-evidence env python3 "$PWD/tests/fixtures/conversation-work.py"
```

It adds three worktrees of the fixture project under `.worktrees/`, gives the
project the remote `https://github.com/octo-org/fixture-app`, turns the
[forge](#an-offline-github) on with five seeded pull requests, and writes
[`conversation-work/frames.jsonl`](../tests/fixtures/conversation-work/frames.jsonl)
as the provider's `fixture-frames.jsonl`, with this fixture's paths. Its turn
replays Claude Code `Edit` and `Bash` calls and their output, in the shape
Claude Code records them:

| Work row | What the transcript did | Forge state |
| --- | --- | --- |
| #41 Key the response cache by locale, **Created**, worktree `fix/cache-key` | edited it, committed, pushed and ran `gh pr create`, then left a test uncommitted | open; checks build passed, lint failed, test running |
| #38 Bound the retry budget, **Pushed**, worktree `feat/retry-budget` | edited it, committed and pushed its branch | open, approved, every check passed |
| #35 Drop the legacy cache flag, **Updated** | `gh pr merge 35` | merged |
| `docs/locale-notes`, a worktree with no pull request | edited `docs/locales.md`, uncommitted | |
| #12 and #29, **Mentioned** | named by URL and by `#29` in prose | merged; closed |

Every Claude turn in the fixture replays the same frames until
`home/.claude/fixture-frames.jsonl` is removed. Running the script again
rewrites the seeds and frames and keeps the worktrees. Commit IDs differ
between sessions, since the fixture project's first commit does, so the
pushed range in the retry-budget output differs between Before and After.
`host::work::transcript::tests` checks that the daemon reads from these frames
the pull requests, branches and edited directories the table names, with
their relations.

An Antigravity or Muse chat can replay the same turn. Start the session with
`--acp`, run the script, then send the chat a prompt that contains the word
`work`. The offline ACP agent reports each step the way Antigravity does: a
shell command with its output, or an edit with its diff. The daemon reads
the chat's work from cagents' own record of the chat, not from a transcript
file. `host::work::items::tests` and `chat::runtime::store::unread::tests`
check that reading.

Mentioned pull requests are listed only once **Show mentioned pull requests**
is on in Settings; it is off by default.

To see the list arrive late, set a [forge delay](#an-offline-github) of
3 seconds right after preparing the fixture, before the project is opened, so
the app knows none of its pull requests yet; the turn's replay asks GitHub
nothing. The inspector then shows only Source control for several seconds
before the Work list replaces it, and since every `gh` call waits, the settled
list takes a minute or two. The steps below capture that loading frame
(`work-loading`), the settled list (`work-populated`) and the list with
Mentioned expanded (`work-mentioned`), then remove the delay. The inspector may
already be open when the chat opens, so they toggle it only when it is not:

```sh
# List mentioned pull requests too; the setting is off by default.
evidence drive key ctrl+comma >/dev/null
evidence drive wait 'App theme' --timeout 15 >/dev/null
# OCR splits the search field's placeholder unpredictably; its icon sits at its left.
search=$(evidence drive --json observe | python3 -c '
import json, sys
runs = json.load(sys.stdin)["runs"]
field = min((r for r in runs if "earch" in r["text"] or r["text"].strip() == "settings"), key=lambda r: r["y"])
print("{},{}".format(round(field["x"] + field["width"] / 2), round(field["y"] + field["height"] / 2)))')
evidence drive click "$search" >/dev/null
evidence drive type Mentioned >/dev/null
evidence drive wait 'Show mentioned pull requests' --timeout 10 >/dev/null
evidence drive click 'Show mentioned pull requests' >/dev/null
# Settings are saved in the background.
evidence env sh -c 'for _ in $(seq 10); do grep -qsx "show_mentioned_pull_requests = true" "$XDG_CONFIG_HOME"/cagents-development*/settings.toml && exit 0; sleep 1; done; exit 1'

evidence env python3 "$tool/tests/fixtures/conversation-work.py" >/dev/null
# From here GitHub answers 3 s late, so nothing about the project's pull
# requests is known before the inspector opens; the replay asks GitHub nothing.
evidence env sh -c 'echo 3 >"$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge-delay"'
evidence cli project open '{fixture}/project' >/dev/null
channel=$(evidence cli --json channel spawn --repository '{fixture}/project' --title 'Locale cache work' |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["channel"]["id"])')
evidence cli --json agent spawn --provider claude --model haiku --surface chat \
    --repository '{fixture}/project' --channel "$channel" \
    --prompt 'Key the response cache by locale, finish the retry budget and land the legacy flag cleanup.' >/dev/null
# The turn has replayed once its answer is in the transcript.
evidence env sh -c 'for _ in $(seq 60); do grep -qs work-answer "$HOME"/.claude/projects/*/*.jsonl && exit 0; sleep 1; done; exit 1'

# Open the chat from its sidebar row, by one word of its title.
evidence drive wait 'Locale' --timeout 30 >/dev/null
evidence drive click 'Locale' >/dev/null
evidence drive wait 'Picking' --timeout 30 >/dev/null
evidence drive hover 1,1 >/dev/null
# Open the pane's inspector (Ctrl+Shift+I) unless it already is.
observed=$(evidence drive observe)
if ! grep -Eq 'Source control|Work [0-9]' <<<"$observed"; then
    evidence drive key ctrl+shift+i >/dev/null
fi
evidence shot work-loading
grep -q 'Bound the retry budget' <<<"$(evidence drive observe)" &&
    echo 'warning: the Work list may have been in place for work-loading' >&2
# Rows arrive one lookup at a time; with every answer 3 s late the settled
# list takes a minute or two.
evidence drive wait 'Bound the retry budget' --timeout 180 >/dev/null
evidence drive wait 'Mentioned 2' --timeout 120 >/dev/null
sleep 3
evidence shot work-populated
evidence drive click 'Mentioned' >/dev/null
sleep 1
evidence drive hover 1,1 >/dev/null
evidence shot work-mentioned
# A seed saved after this should not keep GitHub slow.
evidence env sh -c 'rm -f "$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge-delay"'
```

`$tool` is the checkout `pair` runs its tool from, which must contain this
script. The seeded chat is a live conversation, so it needs no saved seed; a
`save-seed` after the turn keeps its worktrees and forge, but the chat itself
reopens only while its provider home is the seed's.

## A scoped UI caller

Add `start SIDE --ui-scoped-fixture`, or pass `--ui-scoped-fixture` to a remote
`pair`, to exercise permission refusals in the actual UI. The option starts
the fixture's daemon as User, then adds the hardcoded invalid synthetic
`native-evidence-invalid-session` authority only to the UI runner's environment.
It accepts no credential value and forwards no caller authority. `env` and
`cli` commands retain User authority so a scene can seed its private data.
A later `start` without the option removes that selection; `relaunch` keeps
the current session's selection.

Run it through `--remote fw16` from a hosted session. The ordinary inherited
authority refusal still applies before fixture setup. This fixture tests a
rejected synthetic caller, not the permissions of a valid hosted session.
It uses the same offline providers and isolated provider homes as other
native-evidence scenes.

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
no pull requests and no review requests except those seeded below:

| The app runs | It answers |
| --- | --- |
| `gh auth status --json hosts`, with or without `--hostname` | a successful `fixture` account for github.com; `{"hosts":{}}` for any other host |
| `gh repo view --json nameWithOwner,url` | the repository of the checkout's first github.com remote, in gh's order (`upstream`, `github`, `origin`, then the rest), or gh's own error when there is none |
| `gh pr list ... --json FIELDS` | `[]`, or the seeded pull requests below; `--author @me` means `fixture` |
| `gh pr view N` | a seeded pull request, or gh's error for an unknown number |
| `gh pr checks N` | gh's `no checks reported` for a seeded pull request; a seed's checks reach the app through `statusCheckRollup` in `gh pr view` and the queries below instead |
| `gh api graphql` | the seeded open pull requests for the review-request poll and the pull request browser, and their review-request history; empty pages for the browser's search; not found for a pull request's own queries |
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
`statusCheckRollup`; history omits checks. As GitHub does, the board's open
search lists a hundred checks per pull request with their total and a cursor,
and its `CagentsPullRequestChecks` reads the next hundred from there. This exercises the native sidebar's
actual query and decoder without a GitHub account.

For polling evidence, create `home/.config/fixture-forge-trace`. The forge then
appends epoch time, parent PID and operation label to
`home/.config/fixture-forge-requests.tsv` for every command it answers. GraphQL
labels name the operation and repository, and REST labels the method and path,
so daemon Work polling can be counted separately from sidebar and branch
lookups. No query bodies or variables are recorded.

To see what the app shows while GitHub is slow, write a number of seconds,
such as `3`, to `home/.config/fixture-forge-delay`. Every answer then waits
that long first, until the file is removed; anything but a number fails each
call, naming the file. For a conversation's Work list, keep it
around 3 seconds, well under 5: the daemon gives up a pull request lookup
after 5 seconds, including the fixture's own work, and waits 30 seconds or
more before asking again.

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

The pull request browser lists the open seeds in file order, fifty a page, so
53 open seeds give it a second page. An entry may also set
`fixtureReviewRequests`, the number of times the reader was asked to review it
(0 if absent). One or more makes it a review request for the reader, labeled
**Needs your review**; two or more make it **Review re-requested**. A request
beyond the browser's loaded pages appears under those filters as its own
request row.

Seeds still cannot support evidence of the review itself. Opening a seeded
pull request's review asks for its conversation, which the forge answers as
not found. Like any failed GitHub read, that pauses the app's GitHub refreshes
for a while. That failure is exactly what
[an offline cached review](#an-offline-cached-review) needs.

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

## An offline cached review

A pull request review the app has read before reopens from its local cache
while GitHub can't be reached: its files, viewed marks, selected file and any
unposted draft, read only until GitHub confirms the review again.
[`tests/fixtures/offline-review-cache.py`](../tests/fixtures/offline-review-cache.py)
prepares that state. Run it once the app has started, since the cache lives in
the app's own session store, and before opening the review; it takes no
arguments:

```sh
scripts/native-evidence env python3 "$PWD/tests/fixtures/offline-review-cache.py"
```

It commits a base and a head to the fixture project, checks the head out as
the worktree `.worktrees/cache-key` on `fix/cache-key`, gives the project the
remote `https://github.com/octo-org/fixture-app` and turns the
[forge](#an-offline-github) on with #57 **Key the response cache by locale**,
which that worktree heads. It then writes #57's review into the store's
`review_cache` tables in one transaction, as the app saves it:
`docs/caching.md` added and viewed, `src/cache.rs` changed and selected, and
a draft comment on line 23 of `src/cache.rs` that was never posted. The diff's
commits are the project's own, so it renders; the forge answers the review's
own queries as not found, so the review stays **Cached for fixture · read
only**, with the failed `gh` read as its notice. The seed's write order is
outside the app's clock, so the app's next save of the review replaces it, and
running the script again rewrites the cache and keeps the commits. Other seeds
in `fixture-forge-prs.json` are kept.

To open the review as a person does, show the sidebar's search, choose its
**Pull requests** filter and click the `fix/cache-key` row. The draft's
composer sits under line 23; its buttons are below the fold at the default
height, so scroll the diff to reach them. Don't click a diff gutter in a
revision without the fix for #3417: that starts a comment the cached review has
no authority for, and the app panics. A steps file:

```sh
evidence env python3 -I "$tool/tests/fixtures/offline-review-cache.py" >/dev/null
evidence cli project open '{fixture}/project' >/dev/null
evidence drive wait 'Worktrees' --timeout 60 >/dev/null
evidence drive click 206,67 >/dev/null    # the sidebar's search
evidence drive wait 'Pull requests' --timeout 10 >/dev/null
evidence drive click 'Pull requests' >/dev/null
evidence drive wait 'cache-key' --timeout 60 >/dev/null
evidence drive click 'cache-key' >/dev/null
evidence drive wait 'read only' --timeout 30 >/dev/null
evidence drive wait 'empty locale' --timeout 30 >/dev/null
evidence shot offline-review
```

If `Worktrees` is collapsed, the filter shows no `fix/cache-key` row until it
is expanded. `tests/offline_review_cache_fixture.py` checks the rows the
script writes against the tables `src/persistence.rs` creates, and that the
forge lists #57 at the cached head; the native review is what shows the app
reads them.

## A failed automatic terminal completion

Development builds under `native-evidence` refuse a pending terminal's completion
save while `home/.config/fixture-pending-terminal-save-error` exists in
`CAGENTS_EVIDENCE_FIXTURE`. Create the file before opening a terminal. Its worker
starts normally; the fixture refuses only the edit that clears the pane's pending
flag. Remove the file and use the terminal's existing Retry control to exercise
completion of that same worker.

This simulates a save error in the actual daemon and native UI. It does not
simulate SQLite contention or an execution retirement, and contacts no provider.
The fault is absent from builds without the `development` feature.

`tests/fixtures/pending-terminal-save-scene.sh` exercises this refusal in a
native before/after pair: it types into the shell while saving fails, retries
once while the fault remains, then removes the fault and retries saving. It
records the pending flag and the holder-owned shell's PID/start time to
separate worker launches from the UI's interactive PATH probes. The recovered
shot also checks keyboard focus by typing immediately after Retry save.

The scene checks the exact pane/terminal IDs and pending flag before and after
recovery, as well as the original worker's PID/start time and fixture working
directory. It waits for the selected workspace's empty draft and visible terminal
output before typing. `SCENE_SHOT_PREFIX=narrow-` keeps a second layout's images
separate; the app's responsive inspector state is preserved. Single-word OCR
locators tolerate split text runs. The unique `Retry` locator targets the existing
**Retry save** button even when OCR splits that label into two boxes.

## A copy whose text changed

A message's Copy reads its whole text from the daemon's transcript store,
which refuses the read if the item changed after the snapshot the app copied
from. That happens only when a provider rewrites the item within the few
milliseconds a read takes, so development builds under `native-evidence`
refuse every such read as changed while
`home/.config/fixture-text-copy-changed` exists in `CAGENTS_EVIDENCE_FIXTURE`.
The store's sources stay as they are, so the app has nothing newer to read
and says so beside the Copy that was clicked. Remove the file and the same
Copy succeeds. Only the conversation's own items are refused; a subagent's
copies are unaffected.

```sh
scripts/native-evidence env touch '{fixture}/home/.config/fixture-text-copy-changed'
# Click a finished reply's Copy, and capture what it says.
scripts/native-evidence env rm '{fixture}/home/.config/fixture-text-copy-changed'
```

This covers the store noticing the change first. When the app's snapshot sees
it first, the app reads the item again before the store answers; the unit
tests in `text_copy::tests` cover that order. The fault is absent from builds
without the `development` feature, and contacts no provider.

## A copy whose text is unavailable

While `home/.config/fixture-text-copy-unavailable` exists, the same store
refuses every conversation copy as though its cache no longer held the text,
as after the item was trimmed from it. Removing the file lets the same Copy
succeed again.

```sh
scripts/native-evidence env touch '{fixture}/home/.config/fixture-text-copy-unavailable'
scripts/native-evidence env rm '{fixture}/home/.config/fixture-text-copy-unavailable'
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
| `usage-resets` | `codex-usage-resets.py` | reports invented usage and reset credits; consumes update only its private fixture state |

`start` copies the peers and their shared helpers into
`TASK/fixture/libexec/codex`, since the peers load their transport from beside them, creates `home/.codex`, and
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
The `usage-resets` peer also exercises account usage and explicit reset
consumption with invented credits. The other peers leave the app's account and
usage-limit handling unexercised, and no peer verifies live account credentials.

With `home/.codex/fixture-unnamed-child` present, the `tool-rows` peer's
collaboration calls and thread listing report its child without a nickname,
and only the child's own thread read names it Quinn: a child discovered before
Codex named it.

With `home/.codex/fixture-nested-child` present, its thread listing also
reports a subagent the child spawned earlier, Ada, which never runs live. Like
Codex, the peer lists a child it hasn't loaded since it started as
`notLoaded`, and refuses to read Ada until the child's own read has loaded it:
the state a restarted daemon restores nested subagents from.

The `tool-rows` peer also reads an optional `model/list` result from
`home/.codex/fixture-models.json`. Its nonempty `data` array needs a `model`
ID on each entry; `displayName`, `supportedReasoningEfforts`,
`defaultReasoningEffort` and `isDefault` use the app-server's field names.
A new thread reports the requested model or the catalog's default (the first
entry when none is marked), and reports an effort only when the launch chose
one. Model and effort overrides sent with later turns are retained when the
fixture thread resumes. Without the file, it keeps the usual `fixture-mini`
catalog. This exercises catalog and selection handling, without checking
which models or efforts a live account can use.
With `home/.codex/fixture-unconfirmed-model` present, thread start/resume omits
the model field while turns keep their ordinary activity and completion frames.
Remove the marker to report the selected model on the next start/resume.
`home/.codex/fixture-turn-pause` optionally holds a number of seconds from 0
to 300 for the peer's busy turn, instead of its usual six-second pause.
The footer scene uses 20 seconds so native capture can show confirmed running
work without depending on how quickly OCR and screenshot conversion finish.
`home/.codex/fixture-turn-start-delay` optionally holds a number of seconds
from 0 to 30 to hold each `turn/start` response and its turn's notifications.
Codex learns a turn's id only from those, so this shows a message the daemon has
accepted whose turn has not begun, and what is sent meanwhile. The peer accepts
a `turn/steer` naming its active turn. With `home/.codex/fixture-user-items`
present, the peer reports each prompt and steer as a `userMessage` item of its
turn, as Codex does, so the reader's own messages appear in the transcript.
Create `home/.codex/fixture-turn-audit.json` containing `[]` to record the last
16 accepted turn IDs and the actual requested model/effort fields, including
only fields present in the request. This private audit excludes prompt input and
helps compare a native selection with the request received by the peer. Writers
share a sibling file lock and replace the JSON atomically, so concurrent fixture
processes using the same private home retain their records up to that limit.

[`tests/fixtures/chat-footer-scene.sh`](../tests/fixtures/chat-footer-scene.sh)
captures a draft, a started conversation, the open model picker and a selected
high effort for Claude and Codex. Run it in a fresh `--codex tool-rows` session,
as `wide` at 1500×900 or `narrow` at 900×900. Prepare a new task once before
capturing: start and stop a side, run the scene as `prepare` while it is stopped,
then `save-seed`. This removes the bootstrap's private account catalog cache,
so both starts probe the prepared catalogs. For a remote `pair`, use a steps
file containing `CAGENTS_EVIDENCE_TASK=$task
"$tool/tests/fixtures/chat-footer-scene.sh" wide` (or `narrow`). The scene
copies the recorded Claude catalog and writes an invented offline Codex
catalog only into the fixture. It exercises the native controls and the
provider protocol peers, without contacting either provider.

For native sign-in controls, prompt the `child-errors` peer with `auth-required`.
It refuses `turn/start` with an explicit authentication error. Put a whole number
of seconds in `home/.codex/fixture-sign-in-seconds` to keep the fake `codex login`
terminal open while **Signing in…** is visible; `login status` stays immediate.
This exercises the app's recovery controls and login-terminal lifecycle without
authenticating an account or contacting a provider. It does not verify provider
credentials or account changes.

### An automatic account switch

The `tool-rows` peer can play a second Codex account. A home's
`fixture-account-email` names the login its account check reports, so a
second linked home is a different account rather than a login conflict. With
`fixture-rollouts` in a home, a new thread writes a minimal rollout under
`sessions/`, as Codex keeps one; a peer asked to resume a thread it doesn't
know takes it over when its own home has that rollout, as it does once an
account switch has copied it there. It does not replay the earlier turns, so
after a switch the conversation shows only what follows. Give each home its own
`fixture-rate-limits.json`, with `{"ordinaryUsageAllowed": false}` on the
account that should send its conversations elsewhere, and choose automatic
routing with `cli account default codex automatic`.

To make that switch meet a busy daemon, create `.cagents-fixture-hold-preflight`
in the fixture `HOME` and run `cli daemon preflight`; its client gives up
after a few seconds. A debug daemon whose `HOME` is inside a native-evidence
task keeps that preflight in ordinary admission while the file exists, for at
most two minutes, as a slow request would, so a switch sent meanwhile waits at
the control gate for its deadline and is refused there. A marker reading
`exclusive` holds the daemon as a long operation such as an update does, so
even the switch's connection waits. Remove the file to
free the daemon. Release builds and any other home ignore it.
[`tests/fixtures/account-switch-busy-scene.sh`](../tests/fixtures/account-switch-busy-scene.sh)
prepares both accounts and plays the refused switch, sending again once the
daemon is free; see the script for its seed and the exit variant.

A switch the daemon refuses for a reason of its own needs no hold. Its
`diverged` variant writes a different copy of each rollout under the second
account's `sessions/`, as a conversation already continued on that account
would leave, so the daemon refuses to overwrite it before closing anything.
The scene then removes the copies and sends again.

## Usage reset evidence

`tests/fixtures/usage-reset-scene.sh` prepares invented Claude and Codex
allowances in a running native-evidence task. Start with `--codex usage-resets`
on both sides. Run these commands on the build host, with
`CAGENTS_EVIDENCE_TASK` naming that task; hosted agents use plain SSH:

```sh
tests/fixtures/usage-reset-scene.sh seed   # once, before save-seed
tests/fixtures/usage-reset-scene.sh serve  # after each start restores the seed
scripts/native-evidence cli account info --account claude/system --refresh
scripts/native-evidence cli account info --account codex/system --refresh
```

`seed` refuses an existing reset state, creates `.cagents-reset-fixture` under
the private fixture root, and populates each private provider home. Claude has
a Fable grant with two resets, an account grant with one, and an available
recurring session reset; the Fable grant is the next usable grant. Session
usage is 82%, weekly usage 96% and Fable
usage 100%. Codex has two reset credits, a session at 90% and a week at 100%.
Counts, timestamps, identities and messages are invented.

`serve` binds a loopback HTTP listener and writes the Claude home's
`fixture-reset-api.json` with its address. The debug-only Claude reset
transport recognizes that marker under the marked fixture root and sends
only the literal fake bearer `cagents-fixture-only`; it does not read real
credentials. The listener accepts only that bearer and the invented
organization. Like Claude's service, it offers reset programs only to Claude
Code's CLI surface: a usage read whose agent is not `claude-cli/…` gets
`cedar_ember` and `juniper_tide` as ineligible with `ineligible_reason:
"surface"` and no grants. Each served grant reports `percent_used` for the
limits it clears, from the current usage. Its private HOME/runtime lets
native-evidence stop identify and
end it with the rest of the task. Start the listener again after restoring a
seed, since an old port does not identify the new session.

Open Accounts and capture the usage view. Spawn a synthetic Claude chat with
`--model fable`: its initial turn reports the Fable limit and stops, providing
the limit-card comparison. Capture the reset confirmation and cancel it first.
On the after side, a confirmed reset updates `fixture-usage.json`, the count,
and the next fake turn's live usage. A Fable reset preserves account windows;
an account reset preserves Fable. No real reset is consumed. The recurring session route deliberately has no
server idempotency: `session_post_count` records every submitted POST so a
client contract can verify that an uncertain request is never sent again.

For recovery evidence, `usage-reset-scene.sh mode claude disconnect` makes a
claim persist before closing its connection; the refresh sees the true state,
and replaying the original idempotency key spends nothing further. Use
`mode claude error` for a refusal before spending. Codex accepts those same
modes. `mode claude unconfirmed` spends once but answers HTTP 200 with
`result: unavailable` and `reason: reset_unconfirmed`; the same key then
answers `already_used` without another spend. Restore the seed before each independent comparison so both sides
start with the same counts. For a limited account without a reset action, use
`mode claude depleted` or `mode codex depleted`: these set the relevant window
to 100% and all reset counts to zero, without submitting a consume. Claude
turn frames are refreshed too, so the Fable card stays limited without an
available reset. To hit the limit again while retaining the remaining resets,
use `mode claude limited` or `mode codex limited`. This updates the relevant
usage windows without changing reset counts, prior claims or consume counts.
`mode claude fable-unused` sets Fable usage to 0% and clears the Fable wall
without changing grants, so the Fable reset has nothing to refill and is shown
as not usable while the account reset stays available.
Codex then fails a turn only while ordinary usage is blocked; after a fake
reset it emits an ordinary completed assistant reply, so the native flow can
verify that work continues. Mode changes share the service's state lock.

For Claude scope confirmations, `usage-reset-scene.sh next-grant fable|account`
selects one of the two existing grants as `next_grant_id`. The account
grant clears both Session and Weekly; the Fable grant clears only its model
window. The selector preserves usage, counts, claims and identity, and rejects
missing, exhausted, paused or unusable grants. It does not submit a consume.
As in Claude Code, the next grant is only the suggested one: the service
accepts a claim for any usable grant and answers `ineligible` for a paused or
unusable one.

For Claude plan-label evidence, `usage-reset-scene.sh plan NAME` accepts
`pro`, `max5x`, `max20x`, `ultra` or `unknown`. It changes the fake profile's
`organization.plan_display_name`, its tier/type metadata, the debug marker's
optional `subscription_type` and `rate_limit_tier`, and the usage report's
legacy subscription type. Email, organization UUID, usage windows, reset
counts and prior claims stay unchanged; the switch never submits a consume.
Refresh account usage after switching plans. The fake auth status and
`get_usage` responses continue to omit display labels and tier metadata,
matching the installed client; the extra information comes from the fake
profile and cached-metadata marker.

For Claude login-change evidence, `usage-reset-scene.sh login EMAIL` accepts
only an address at a reserved example domain, such as `other@example.com` or
the seed's `fixture@example.test`, and changes
only the fake profile and auth-status email, plus the marker's email when
present. Organization UUID, plan, usage windows, reset counts and prior claims
stay unchanged; it never submits a consume. Refresh account usage afterwards.

Pro reports `Pro`. Max cases report the label `Max` with the known cached tier
IDs `default_claude_max_5x` and `default_claude_max_20x`. Ultra reports `Ultra`
while its legacy subscription remains `max`; its profile uses the confirmed
`default_claude_max_20x` tier, `claude_max` organization type and null
`seat_tier`. The unknown case reports `Fixture Horizon`
with the invented `fixture_horizon_unknown_tier`, also retaining legacy
`max`, so a reader can verify that unfamiliar labels are not collapsed into
an older plan. The unchanged default scene reports `Max` without a tier.
Plan switches work before or after starting the fake service and retain its
current loopback address.

The service and Codex peer share `fixture-reset-state.json` files guarded by
file locks. Their offline contracts run with
`/usr/bin/python3 -I -B tests/usage_reset_fixture.py -v`. These verify fake
read/consume behavior and transport ambiguity; they do not qualify the live
Claude endpoint, real Codex credits or the native UI.

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

## Resume and retirement fixtures

| Need | Helper |
| --- | --- |
| A populated saved Claude conversation before starting a provider | [`tests/fixtures/claude-resume-history.py`](#saved-claude-history) |
| A permanent startup refusal for an already retired synthetic Claude actor | [`tests/fixtures/native-retired-ownership.py`](#a-permanent-retired-actor-refusal) |
| A pane Close the registry refuses and keeps the pane through | [`.cagents-fixture-refuse-close`](#a-refused-pane-close) |
| Saved Claude terminals after a reboot, one of them cleared with `/clear` | [`tests/fixtures/cleared-terminal-recovery.py`](#saved-claude-terminals-after-a-reboot) |

### A refused pane close

A debug daemon refuses the next pane Close as it refuses one whose pane
changed while closing, when its `HOME` lies inside a native-evidence task and
holds `.cagents-fixture-refuse-close`. The marker is removed, so the Close
after it goes through. Create it between steps, for example with
`scripts/native-evidence env sh -c 'touch "$HOME/.cagents-fixture-refuse-close"'`,
then close a pane to see the window keep it and say so.

### Close a hosted legacy Codex actor without removing its pane

The `native-codex-close` example uses the production public control client to
publish, prepare and close one exact legacy Codex actor. This lets the still-saved pane
exercise a normal reconnect refusal after a UI relaunch. Build it through
`scripts/cargo-gate build --locked --no-default-features --example native-codex-close`
before taking the native lock, against the same protocol as the fixture daemon.

Run it only through the task's genuine-user `native-evidence env`, over plain SSH
when hosted. It refuses inherited `CAGENTS_AGENT_AUTHORITY` and requires that the
task marker, HOME, runtime and socket belong to the same private fixture. The
`prepare TARGET.json` mode takes `task`, `socket`, `scope`, `pane`, `native` and
`thread` fields, using the IDs of the already displayed ordinary user pane.
Save its JSON output and pass that unchanged to `close PREPARED.json`.

Start the ordinary empty native session first; `native-evidence cli` requires a
live session. For an initial legacy fixture, create a new channel with
`native-evidence cli channel spawn --repository "$task/fixture/project" --title 'Codex Fresh fixture'`,
where `task` is the absolute task directory on the execution host.
Use its channel ID for `scope` and new pane/native UUIDs in the target. Start
only your own isolated Codex listener at
`TASK/old/cagents-codex-NATIVE_UUID/control.sock`. `publish TARGET.json` inserts
one saved descriptor through the normal public model edit, requiring an existing
fixture channel, empty view and hosted-chat inventory, and no agent or resource
identity collision. It fixes the recipe to the private fixture Codex executable,
that legacy remote and the quoted `high` reasoning option. The descriptor has no
host proof, listener ownership or retained capability. Normal UI activation
obtains the real daemon proof and starts the conversation; the helper does not
inject a runtime or start an actor. Once the pane and inventory converge,
`prepare` captures the exact issued descriptor for the checked close.

For a read-only view after Fresh, run `observe TARGET.json` with the same scope
and pane ID. It authenticates as the fixture user, reads the workspace model and
chat inventory, and reports the actual pane, model identity, current native ID
and all inventory entries matching that current ID. The declared old native ID
and thread remain in `target` as request context; they do not constrain this
observation. An empty matching inventory remains an empty array. Observation
requires exactly one canonical Codex pane, but permits its thread, native ID and
owned-listener descriptor to change. It neither starts or attaches a provider
nor proves readiness, confirmed thread identity or listener ownership. These
reads are separate public requests, not an atomic model/inventory snapshot.

The `prepare` and `close` modes still require the saved pane and live inventory
to agree exactly, including the daemon-issued proof and complete recipe. They
reject moved, replaced,
ambiguous or managed-agent targets. Close uses that unmodified listed descriptor,
requires the checked `ChatClosed` acknowledgement, then verifies that inventory
no longer contains the actor and that the saved pane is unchanged. It does not
edit runtime state, recipes, proofs or the saved model. An error after Close can
mean the actor closed but a postcondition failed; inspect the evidence rather
than repeating the mutation.

An independently started app-server is separate from the hosted actor. Stop and
reap only the fixture server you own, recording its identity and socket absence,
before relying on its exit. A later Fresh completion for an unowned legacy
descriptor may use the existing absent-inventory close policy; it is not an
additional retained-close or execution-ended receipt.

### Saved Claude history

Seed a fresh private fixture with a parent-linked conversation before its first
native start:

```sh
python3 tests/fixtures/claude-resume-history.py /path/to/private/fixture --turns 50
python3 tests/fixtures/check-claude-resume-history.py
```

The generator writes 100 invented messages for 50 turns and prints the exact
thread, transcript and project paths as JSON. It launches no provider and uses
no credentials. It refuses an existing provider home and aliases of the real
account's Claude home. Use a fresh evidence task when its provider home already
exists. Save the stopped fixture with `native-evidence save-seed` so both sides
restore the same transcript and physical provider-home identity, then open its
project and choose **Resume conversation** in the app.

### Saved Claude terminals after a reboot

[`tests/fixtures/cleared-terminal-recovery.py`](../tests/fixtures/cleared-terminal-recovery.py)
prepares a seed holding two saved Claude terminals, each launched as
`claude --resume ID`, to restore as they are after the machine restarts. In
one of them, `/clear` has moved Claude Code to a new conversation that has no
transcript yet. The other is still in the conversation it resumed. The
terminals run the synthetic provider's
[interactive form](chat-composer-fixture.md), so the daemon's real foreground
observation records each one's recovery hint. The script launches nothing
itself.

From the build host's User shell, in a fresh task with `TASK` its directory:

```sh
helper=tests/fixtures/cleared-terminal-recovery.py
scripts/native-evidence --task "$TASK" start before
python3 -I "$helper" history "$TASK/fixture"
scripts/native-evidence --task "$TASK" cli project open "$TASK/fixture/project"
scripts/native-evidence --task "$TASK" drive click main --settle 2
scripts/native-evidence --task "$TASK" cli terminal spawn --cwd "$TASK/fixture/project" -- \
    claude --resume a1b2c3d4-1111-4111-8111-000000003390   # prints the terminal ID
scripts/native-evidence --task "$TASK" cli terminal spawn --cwd "$TASK/fixture/project" -- \
    claude --resume a1b2c3d4-2222-4111-8111-000000003390
scripts/native-evidence --task "$TASK" cli terminal write FIRST_TERMINAL_ID $'/clear\r'
python3 -I "$helper" inspect "$TASK/fixture"   # hint_version 2 for the first, 1 for the second
scripts/native-evidence --task "$TASK" stop
python3 -I "$helper" reboot "$TASK/fixture"
scripts/native-evidence --task "$TASK" save-seed
```

The window adopts each CLI-spawned terminal as a pane. Wait until `inspect`
shows the hint versions above before stopping: observation and saving run in
the background. `reboot` stands in for a restart. Each record of the current
kernel boot in the stopped fixture's stores, including the panes' terminal
proofs and the session store's last boot, then names an earlier boot instead.
Nothing else changes. It refuses while anything holds a store open, and
writes `cleared-terminal-reboot.json` once. Every later `start` restores the
two panes as **Terminal unavailable**, and **Retry** relaunches each one.
The terminal then shows the conversation it resumed, or Claude Code's
conversation picker.

For the pane recovery refuses, spawn a third terminal before stopping, with
an option recovery doesn't know beside the conversation it resumes, and
`/clear` it too:

```sh
scripts/native-evidence --task "$TASK" cli terminal spawn --cwd "$TASK/fixture/project" -- \
    claude --resume a1b2c3d4-2222-4111-8111-000000003390 --fixture-unrecognized-option
scripts/native-evidence --task "$TASK" cli terminal write THIRD_TERMINAL_ID $'/clear\r'
```

`inspect` shows hint version 2 for it too. After the reboot its pane offers
**Open new terminal**, with **Retry** disabled. Left uncleared, its hint is
version 3, which confirms the conversation the command names, and **Retry**
runs the command again from the beginning, as it does for the same option
without `--resume`.

### A permanent first-start refusal

[`tests/fixtures/native-account-registry.py`](../tests/fixtures/native-account-registry.py)
creates an invalid private accounts registry so an ordinary first **Send** can
exercise a permanent startup refusal. It prepares an empty home, then faults
only a registry whose Claude default was selected through the real public CLI.
It neither registers an account nor changes a pane, recipe, proof or runtime.
No provider or daemon is launched by the helper.

Use your own isolated `native-evidence` task from a genuine User shell on the
build host. The fixture's HOME, config and data directories must already exist.
Prepare the home, then link it and choose the exact account ID returned by the
CLI:

```sh
python3 tests/fixtures/native-account-registry.py prepare "$TASK/fixture"
scripts/native-evidence --task "$TASK" cli account link claude \
    "$TASK/fixture/data/cagents-development/accounts/claude/55555555-6666-4777-8888-999999999999"
# Substitute the actual claude/UUID returned by account link:
scripts/native-evidence --task "$TASK" cli account default claude claude/RETURNED_UUID
```

This is a linked account in the isolated installation's managed namespace,
not a sign-in or imported credential. The public link command does not check
login or execution readiness. Use the offline fixture provider only. Verify
that the ordinary new draft actually selected this account and captured that
exact `CLAUDE_CONFIG_DIR`; a project override can differ from the global
default. The helper verifies the registration and default, not the UI choice.

```sh
python3 tests/fixtures/native-account-registry.py fault "$TASK/fixture"
# Send the invented first message once and capture the refusal and recovery.
python3 tests/fixtures/native-account-registry.py restore "$TASK/fixture"
```

Malformed TOML produces `InvalidData` during the selected managed home's
startup authorization. A System-default Claude launch skips this registry
check, so corrupting the file alone does not establish the intended failure.
The existing public account selection must precede the fault.

The helper refuses inherited agent authority, symlink aliases, real provider
homes, unsafe ownership or modes, and ambiguous or unselected registrations.
It takes the application's `accounts.toml.lock`, exclusively saves a mode-0600
`fixture/original-account-registry.json` backup, and compares the registry's
exact inode, owner, mode and bytes before writing invalid TOML in place.
Restore accepts only the same helper revision, fixture and home identities,
registry identity, and exact fault bytes. It restores the original bytes and
writes `fixture/restored-account-registry.json`. It never overwrites an
unknown replacement or rearms a used backup.

The registry write and JSON receipt are separate operations. A command can
fail after changing the file. Preserve its error and backup, inspect the exact
current bytes and identity, and stop the task if its state is uncertain. Do
not rerun blindly, delete a backup, or treat a missing restore receipt as proof
that the file was not restored. The lock serializes cooperating application
writers; it does not authorize concurrent manual edits.

The focused checks use invented temporary directories and TOML only:

```sh
python3 -I -B tests/native_account_registry.py -v
```

### A permanent retired-actor refusal

[`tests/fixtures/native-retired-ownership.py`](../tests/fixtures/native-retired-ownership.py)
creates a deliberate saved-recipe conflict so the real daemon can refuse a
startup permanently. Use it only in your own isolated `native-evidence` task,
after the synthetic Claude actor has exited and normal daemon retirement has
completed. The caller must establish that its provider process is reaped;
the helper checks the saved retirement receipt, not kernel process liveness.
It does not retire actors, stop processes, acquire a native lease or launch a
provider.

From the build host's genuine User shell, with the task's current fixture and
exact invented conversation UUID:

```sh
python3 tests/fixtures/native-retired-ownership.py \
    ~/.local/state/cev/TASK/fixture CONVERSATION_UUID
# Exercise the pane's normal Retry once; the daemon reads the conflicted recipe.
# Capture the diagnostic and available controls, then undo only this fault:
python3 tests/fixtures/native-retired-ownership.py \
    ~/.local/state/cev/TASK/fixture CONVERSATION_UUID --restore
```

The helper recognizes only schema 22 shared views or schema 23 to 29
normalized panes with descriptor format 1. Schemas 24 to 29 add tables beside
those panes without changing their columns. It requires one current committed Claude pane
for that exact thread, matching ownership source and proof, and a completed
retirement. It appends one `--fixture-recipe-mismatch` argument to that retired
ownership row using SQLite compare-and-swap. The card, transcript, provider
home and retirement receipt stay as they were. This is synthetic database
fault injection, not a claim that a provider produced the conflicting recipe.

Before changing the row, it exclusively creates a private mode-0600
`fixture/original-retired-ownership.json` backup. Restore accepts only that
helper revision's recorded original and faulted strings, the same database
inode, owner and mode, and unchanged pane, retirement and faulted row. It
restores that one row by compare-and-swap and writes
`fixture/restored-retired-ownership.json`; repeated or stale restores refuse.
Injection is once per fixture. Do not delete the backup or edit it to rearm a
fault or get past a refusal.

The SQLite change and JSON report are separate writes. If either command
fails, inspect its backup, report and exact ownership row before taking any
further action: a missing report does not prove the database was unchanged.
Do not rerun blindly, overwrite the backup or relax a guard. Stop your task
and use a fresh fixture when its state cannot be established safely.

The offline checks open only invented in-memory SQLite databases or temporary
backup files, and need neither Cargo nor a native session:

```sh
python3 tests/fixtures/native-retired-ownership.py --self-test
python3 tests/native_retired_ownership.py -v
```

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

`activity` opens its own named workspace and loads
[`activity.jsonl`](../tests/fixtures/reasoning-row-scene/activity.jsonl): an
empty thought, a command and a background command, then a second empty thought
that stays live for 30 seconds while that command group is still running, then
a newer command that runs for 25 seconds in a group of its own. It captures the
working turn at both points, waits for the answer, and captures the finished
turn.

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" activity
```

`agents` has the lead start a background command and three background
subagents, one of which finishes before the lead answers, so two subagents
and the command are still working after the lead has finished. `plan` needs a session started with
`--codex tool-rows`: its Codex turn publishes a plan with a step in progress
beside the running tool, collaboration and event rows, stays working for 40
seconds through `CODEX_HOME/fixture-plan`, then finishes.

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" agents
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" plan
```

For a short comparison against main after the background-notice changes, run
`refresh-core` then `refresh-notices` in one fresh 1200x900 native session on
each revision. Build both revisions before starting either session, use
`--ui-trace`, and keep the same scale and theme. These modes capture seven
states: working reasoning with finished durations, expanded failed/running
commands, narrow working reasoning, collapsed notices, expanded background
command outcomes, a full monitor event, and expanded finished/failed reports.

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" refresh-core
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" refresh-notices
```

`refresh-core` uses the original reasoning frames and waits for the latest live
line before capturing. Its narrow state adds an unsent blank thread, as below.
`refresh-notices` opens a separate named workspace and loads
[`notices.jsonl`](../tests/fixtures/reasoning-row-scene/notices.jsonl), derived
from the delivery-notices fixture. It preserves successful and failed
background command notifications, extends the monitor log beyond its preview,
and adds a failed Agent report beside the finished report. A peer message
mentions a failure without being a failed report. Its explicit synthetic
sender ID also exercises sender captions. Every call, acknowledgement and
notification is recorded input: no command, child agent or monitor executes,
and no live provider is contacted.
All provider files stay under the evidence task's private home.

Allow about five minutes for the seven captures, then reserve three minutes
for live review. Hand the session to the reviewer by six minutes even if a
locator needs manual completion. Check whole-row targets, keyboard disclosure
retention,
hover/fade continuity and wheel scrolling before stopping. Keep bounded
CPU/RSS samples and the task's UI trace; this fixture has one working actor,
one finished actor and an unsent draft, not a large concurrent workload.

If Before filters empty reasoning, its failed command shares the preceding
mixed step group; After keeps a separate group boundary. The expanded outcome
screens then show different surrounding disclosures and should be labeled as
supplementary states, not presented as a matched pair. The other six states
still provide matching comparisons. During the refreshed native run, the
notice title's leading icon contaminated its OCR spans, and an exact filename
wait failed despite the command being visible. The locators now use the
distinguishing interior words `pool result` and `summary`; the latter does not
depend on how OCR splits filename punctuation.

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
270px each. It captures reasoning and the expanded mixed tools at that width.
The blank draft sends no prompt and starts no provider.

`web` can also run immediately after a fresh `start --codex tool-rows`,
without a core conversation. It creates a named workspace through the CLI,
spawns its synthetic Codex actor explicitly there, and opens that workspace
through the native command palette. Its dedicated app-server `webSearch` item
contains two invented results. It captures collapsed and expanded rows at the
550px docked chat width. No web request is sent.

Keep the native window at 1200x900: below the docked inspector's minimum
workspace width it becomes an overlay that hides the transcript. Run `wide`
then `narrow` once per session. The ancillary modes `web`, `coordination` and
`diagnostics` each use a separate named workspace and can run independently;
they need no core reasoning actor or close request. For a remote `pair`, its
steps file contains:

```sh
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" wide
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" narrow
CAGENTS_EVIDENCE_TASK=$task "$tool/tests/fixtures/reasoning-row-scene.sh" web
```

For the neighboring disclosure controls, run `coordination` and `diagnostics`
in that same session or in a fresh ancillary-only session. `coordination`
atomically stages
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
nested compositor's private clipboard, leaving Details open for live review.
Neither mode reaches the host clipboard.

The OCR locator joins adjacent spans on the same visual line, so split control
labels and end markers still identify the intended row. Repeated outer/nested
rows keep distinct indices, and separate panes are never joined. Run its
source contracts with
`python3 -I tests/fixtures/reasoning-row-scene/test_locate.py`.

All provider settings and transcripts stay inside the task's private fixture;
no live model, provider account or real provider home is used. Stop afterward:
the final thought deliberately stays active for 600 seconds. Run the ancillary
scenes separately when a longer live review is needed. The background command
is a recorded protocol acknowledgement, not a sleeping process.
`--ui-trace` retains UI-thread summaries and calls over 50ms. Compare these and
bounded CPU/RSS samples during the same captures, and exercise hover/scroll
while the latest-line delta arrives. This short fixture has one working
conversation, a blank draft and later a second synthetic conversation; it does
not establish performance for long histories or many concurrent agents.

## Adding a scene to evidence

`examples/shared-removal-fixture.rs` scripts a competing window for shared
Close races. Build it with `scripts/cargo-gate build --locked
--no-default-features --example shared-removal-fixture`, then copy the executable
to the evidence host. Run it through `native-evidence env`, after opening the
fixture project, with the side's protocol number and `unconditional` for the
old ID-only removal or `conditional` for source-checked removal. It refuses an
environment outside native-evidence.

The fixture creates a channel named **Shared Close race**, a repository pane
and a short-lived shell pane. It captures Close, releases the original worker,
replaces the terminal in the same card, then delivers the delayed removal at
the new revision. The unconditional case must remove the replacement; the
conditional case must preserve it. Open the channel and capture the settled
state. This exercises the real daemon's mutation API and the native UI's
projection, using fixture shells and no model provider. The competing window
and delayed request are scripted; unit tests cover the UI's capture and queue.

For a real native stale Close gesture, run `prepare-live` instead. Open **Native
Close race**, open the original terminal's Close confirmation, then run
`replace-live` before confirming it. The fixture stops its own original shell
and publishes another worker under the same terminal ID and recipe. Confirm
the old native dialog and inspect the surviving replacement and refusal state.
The private scene file prevents delivering the replacement twice. These modes
use a sleeping fixture shell and no model provider; stopping native-evidence
cleans up its workers. Exercise a fresh native Close afterward as well.

For a saved completed pane with no resident worker, use `prepare-completed`.
The helper starts, finishes and releases its fixture shell before seeding its
ended descriptor. Prepare it through `env` while the UI is stopped, then use
`start SIDE --keep-state`: a running window can adopt the temporary worker
before the helper places its saved pane. Open **Shared Close race** and close **Original terminal**
once. Its pane should disappear without starting a replacement or needing a
retained final screen. This exercises the completed-source absence proof;
fixtures do not exercise live provider cleanup.

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

## An offline ACP agent

[`tests/fixtures/fake-acp-agent.py`](../tests/fixtures/fake-acp-agent.py)
speaks the Agent Client Protocol over stdio the way Antigravity's `agy acp`
does, with its configuration options for model, thinking level and mode and
its older `models` and `modes` lists. Run as `muse...` or with
`FAKE_ACP_SHAPE=muse`, it copies Muse 1.4.24 instead: its four modes, a
`-contributor` model beside each ordinary one, a reasoning option only on
`muse-spark-1.2` (offering only `xhigh`), a two-word session name sent as
soon as a session opens, and usage with no context size. It ignores its
arguments, so a launch recipe written for the real agent runs it unchanged,
except Antigravity's `--app-data-dir DIR`: like `agy`, a process started
with it keeps its conversations apart from the ones it lists otherwise, so
the account check's own sessions are not listed as the reader's.
It contacts no model, account or network.

In the Muse shape, three account commands answer the way `muse` does, so the
fixture can stand in for `muse-upstream` on a fixture `PATH`:

| Command | The fixture |
| --- | --- |
| `usage --json` | prints an allowance, Muse's no-allowance report while `no-allowance` is marked, or exits 1 with `not logged in` when signed out |
| `logout` | signs out: every later `session/new`, `session/load` and prompt answers Muse's `log in to Muse before opening a session` |
| `login` | prints a made-up device code, waits `FAKE_ACP_LOGIN_SECONDS` (default 2), and signs back in |

Signing out is a `signed-out` file under `FAKE_ACP_STATE`, so it lasts
across processes until `login`. With `FAKE_ACP_AUTH_FILE` set, `login` also
writes that file and `logout` removes it, as Muse keeps its `auth.json`.

It keeps sessions as JSON lines under `FAKE_ACP_STATE`, so `session/load`
replays a conversation from an earlier process and `session/list` lists
it, newest first, with its directory, title and last activity. Point it at
the fixture's own directory; without it, sessions go to a directory in the
system's temporary directory.

With `FAKE_ACP_PRESENCE=1`, it locks a conversation while it has one open,
where the real agent does under `$HOME`: Antigravity's
`~/.gemini/antigravity-cli/presence/ID.lock`, which stays behind unlocked
once it closes, and Muse's `.session.lock` in the conversation's dated
directory under `~/.local/share/muse/sessions`. The Resume conversation
picker reads these locks to say a conversation is open elsewhere. Set it
only for a `$HOME` that is the fixture's own; `fixture-acp.sh` sets it when
that is so.

The first of these words a prompt contains chooses what it does:

| Word | The agent |
| --- | --- |
| `tools` | updates a plan, runs `ls -1` and edits `notes.md` with a diff |
| `work` | replays the [conversation work](#conversation-work) turn: its messages, shell commands with their output, and edits with their diffs |
| `permission` | asks to edit `config.toml` with allow once, allow always and reject options, then acts on the answer |
| `hold` | runs a long step and thinks until stopped |
| `garbage` | writes a line that is not JSON between two parts of its answer |
| `files` | asks the client to read a file and reports the error it got |
| `mode` | moves to the next mode and reports it |
| `title` | names the conversation |
| `refuse` | ends the turn as a refusal |
| `auth` | answers that sign-in is required |
| `allowance` | refuses the turn as a login with no usage allowance |
| `/compact`, at the start | compacts the conversation |

Anything else gets a thought and `Fake answer to: ...`. These variables add
failures:

| Variable | Effect |
| --- | --- |
| `FAKE_ACP_AUTH=initialize`, `new`, `load`, `list` or `prompt` | that request answers that sign-in is required |
| `FAKE_ACP_LIST_PAGE=N` | `session/list` answers `N` conversations a page, each with the `nextCursor` of the next |
| `FAKE_ACP_NO_LOAD=1` | the agent does not offer to load a session |
| `FAKE_ACP_NOISE=1` | a banner line that is not JSON comes before the first reply |
| `FAKE_ACP_REPLAY=N` | loading a session first replays `N` invented exchanges |
| `FAKE_ACP_LOG=PATH` | every frame the agent receives is appended to `PATH` |
| `FAKE_ACP_WORK=PATH` | the Claude Code transcript a `work` prompt replays. The installed `agy` and `muse-upstream` read the fixture's `home/.claude/fixture-frames.jsonl`, which `conversation-work.py` writes |
| `FAKE_ACP_AUTH_FILE=PATH` | the sign-in record Muse's `login` writes and `logout` removes; only ever a fixture's own file |

These files under `FAKE_ACP_STATE` switch states that last across processes:

| File | Effect |
| --- | --- |
| `signed-out` | every new or loaded session and every prompt answers that sign-in is required |
| `no-allowance` | every prompt is refused as a login with no usage allowance, and Muse's `usage --json` reports none |
| `hang-probe` | a session opened by the account probe (`--app-data-dir`) never answers, as one that times out |

Run with `--app-data-dir DIR`, as the Antigravity account probe runs
`agy acp`, it saves each new session's files under `~/.gemini/DIR` the way
agy does (`conversations/ID.db`, `brain/ID/`, `annotations/ID.pbtxt` and
`implicit/ID.pb`), so a test can check that a probe leaves none behind.

`native-evidence start SIDE --acp` (or `pair ... --acp`) installs it into the
fixture as `bin/agy` and `bin/muse-upstream`, through
[`tests/fixtures/fixture-acp.sh`](../tests/fixtures/fixture-acp.sh), with a
`bin/muse` that runs `muse-upstream` behind root options as a reader's own
wrapper might. Each keeps its sessions in the fixture's `acp/agy` or
`acp/muse`, and logs the frames it receives to `frames.log` there, so a check
can read which settings and prompts cagents sent. Muse starts signed in, with
the fixture's `config/muse/auth.json`. While `acp/agy/signed-out` exists,
`agy` answers every new or loaded session that sign-in is required, and the
app's sign-in, `agy acp --login`, removes it:

```sh
evidence env touch '{fixture}/acp/agy/signed-out'
evidence env touch '{fixture}/acp/muse/no-allowance'
```

A conversation started outside cagents is one the fake agent saved without
the app: run the fixture's `agy acp` (or `muse-upstream acp`) and open a
session with `session/new`, or write an entry into `acp/AGENT/index.json`
(`{"ID": {"cwd": DIRECTORY, "title": TITLE, "updatedAt": RFC3339}}`), which
`session/list` lists as it is. `session/load` replays it only once
`acp/AGENT/sessions/ID.jsonl` holds its session updates, one a line; an
empty file loads it with no history.

A plain start installs neither, so Antigravity and Muse are not installed.

The ACP adapter's runtime test in `src/chat/runtime/tests.rs` drives it
through a real pipe.

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

## An update restore in progress or held

After an update, the new daemon restores the conversations the old one ran.
While it restores, and if a restore fails and is held for recovery, it
admits only some requests, and windows and panes have to say why. To show
those states without a failed installed update, create
`.cagents-fixture-startup-recovery` in the fixture `HOME`:

```sh
scripts/native-evidence env sh -c 'echo restoring >"$CAGENTS_EVIDENCE_FIXTURE/home/.cagents-fixture-startup-recovery"'
scripts/native-evidence env sh -c 'echo "retained: Codex server did not open its local socket within 15 seconds" >"$CAGENTS_EVIDENCE_FIXTURE/home/.cagents-fixture-startup-recovery"'
```

`restoring` reports a restore still in progress. `retained: REASON` reports a
restore held for recovery with that reason, and `retained` alone reports a
Codex listener that never opened its socket. While the file exists, a debug
daemon whose `HOME` is inside a native-evidence task answers `StartupState`
that way and gates requests as it does for a real startup recovery. The file
is read on each request, so it can also be written or removed while the
session runs. Remove it to end the hold; the daemon then reports ready, as one
that finished restoring does. No owner is actually retained, so this shows
what clients are told and admitted, not how a real restore adopts its owners.
Release builds and any other home ignore it. Save a seed with open chats
first, so panes have saved conversations to wait for.

### A conversation the update stopped

When a conversation's restart after an update fails, the daemon stops it and
refuses to reattach its pane until the conversation is started again. To show
that state, start a session with `--codex tool-rows`, create `fixture-rollouts`
in the fixture's `home/.codex` so the peer can resume the conversation later,
and open a Codex conversation. Close the app window, so no window reopens the
conversation by itself, then create `.cagents-fixture-update-stopped` in the
fixture `HOME`, optionally naming the provider's reason, and stand in for the
update: checkpoint and stop the daemon, and start the next one, which restores
from that checkpoint:

```sh
scripts/native-evidence env sh -c 'echo "Codex server exited before opening its local socket" >"$HOME/.cagents-fixture-update-stopped"'
scripts/native-evidence cli daemon prepare-upgrade --yes
scripts/native-evidence cli daemon start
scripts/native-evidence cli daemon status
scripts/native-evidence env sh -c 'rm "$HOME/.cagents-fixture-update-stopped"'
```

While the file exists, every conversation a debug daemon in a native-evidence
task restarts from an update's checkpoint fails with that reason before
anything starts. The daemon stops it and records it as it does a real failed
restart, and `daemon status` names it. Retained conversations, which are
reattached rather than restarted, are unaffected.

Then `relaunch --restore-existing-only` opens the recovery window that `daemon
recover-update` opens, which only reattaches; an ordinary window starts the
conversation again by itself. To capture both sides from the same stopped
conversation, `stop` the session instead, keep the fixture with `save-seed`,
and start each side with `--restore-existing-only`, so its first window is
that recovery window too.

## A read-only native update review

Development UI builds under `native-evidence` read
`home/.config/fixture-update-review.json` in `CAGENTS_EVIDENCE_FIXTURE`.
Copy `tests/fixtures/update-review.json` there with mode 0600, then start the
session (or wait for the active review's next status poll). The example includes
running work, a blocked conversation, a daemon notice and a paused terminal.
The file must contain a job named `fixture-update-review`: either awaiting
confirmation with a daemon preview, or ended (`Failed` or `Cancelled`) with no
confirmation, to show how a review ended. Decisions fail before looking up an
installation. While the session runs, replace the blocked review with one whose
blockers are empty and whose sessions continue to show the update becoming
available at the next status poll, or with an ended one to show the window that
watched the review receive its outcome. Remove the file to end the fixture
review.

To show what the header offers when a newer release exists, also copy
`tests/fixtures/update-release.json` to `home/.config/fixture-update-release.json`
(mode 0600) before starting. The development build's update check, which
otherwise never runs, reports that release as available at startup and on
"Check again". Under native evidence, `apply` refuses every release, so
"Update to" and "Retry update to" fail without installing anything.

This loads data into the actual native dialog and supports scrolling and Copy
details. It does not run providers, inspect a live update inventory, perform an
update, or exercise cancellation/recovery. The grouped Paused heading belongs
to the CLI review. The native dialog currently omits saved stopped panes, so
the fixture's paused terminal does not appear in that dialog.

## An update that stopped work in progress

An ended update job in `home/.config/fixture-update-review.json` can carry
the census's `in_flight` outcomes, which the window reads at startup to offer
resuming the conversations whose work didn't survive.
[`update-resume-scene.sh`](../tests/fixtures/update-resume-scene.sh) builds
one over real offline conversations. Run it as a steps file: `prepare` in a
running session opens the fixture project and starts four Claude agents in a
"Release checks" channel; then `stop` and `save-seed`. `shots`, in each side's
session, records a completed update in which one conversation was restarted
without its subagent and background command, one had its turn interrupted,
one was ended with a queued message, and one kept running. It then restarts
only the app window, which reads the update as a window opened after one
does, and captures the offer. Where the offer exists, it resumes one
conversation, shows the message that conversation received, and dismisses
the rest. A session's start reopens the seeded conversations under new ids,
which is why the update is recorded inside the session that shows it. The
offer is made once per update: what was resumed or dismissed is kept in the
record every window shares, `update-resume.sqlite3` in the data directory,
until the next `start` restores the seed.

## A refused shared-view seed

[`seed-refusal-scene.sh`](../tests/fixtures/seed-refusal-scene.sh) creates an
offline Claude conversation with an unsent launch-note draft. Its `refuse`
step closes only the UI, keeps the daemon and provider alive, then uses
[`seed-refusal-registry.py`](../tests/fixtures/seed-refusal-registry.py) to
remove that channel's shared view and, on a registry from before schema 27,
its active channel-work placement mappings. It keeps the monotonic clocks,
actor/history records, window retention pins and other scopes. The helper
then adds a trigger that published-schema admission does not recognize. While armed, the real registry can refuse all writes, before
the trigger's channel-conditional body runs. The native notice reports the
registry's schema-mismatch reason, not the trigger body's SQLite sentence.
The fixture does not inject UI state, replace transport or weaken schema checks.
Dropping the trigger restores admission; the selected panes can register again.

Run the scene on the native-evidence host, with both fixture scripts in the
same directory. When comparing a revision that predates the scripts, copy them
into the evidence task's `tools/` directory and use that copy on both sides.
Keep the helper outside the before and after checkouts so their recorded source
stays unchanged. Build both revisions before taking a native slot.

```sh
export CAGENTS_EVIDENCE_TASK=$HOME/.local/state/cev/seed-refusal
export CAGENTS_EVIDENCE_TOOL=/path/to/checkout/scripts/native-evidence
scene=$CAGENTS_EVIDENCE_TASK/tools/seed-refusal-scene.sh

# Once, with a native session running; every later start restores these bytes.
bash "$scene" seed
"$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" stop
"$CAGENTS_EVIDENCE_TOOL" --task "$CAGENTS_EVIDENCE_TASK" save-seed

# After starting either side at the comparison's width:
# Observe the closed inspector's notices icon and set its point. Passing the
# point to refuse clears unrelated transient headless-desktop notices.
export NOTICE_CONTROL_POINT=$observed_point
bash "$scene" refuse wide
# On the fixed side:
bash "$scene" notice wide
bash "$scene" retry wide
```

`refuse` captures the saved draft before opening notices. `notice` checks the
existing warning control's tooltip and captures the refusal and Retry action.
`retry` removes the fixture refusal, presses Retry and waits for the saved pane
to reopen. Repeat with `narrow` in a new session at the narrower viewport;
both widths restore the same seed. To avoid taking another native slot, a
1500×900 session can instead use `native-evidence nested niri msg action
set-column-width 900` and return to `1500`; first confirm the actual app-window
size with `drive observe`. Capture the full native window. The manifest retains
the requested headless viewport and separately records the actual image size;
report the latter for a resized capture. Apply identical geometry on both sides.
The notices icon's position depends on that
viewport and the inspector state, so observe it instead of reusing a guessed
point. Stop immediately after capturing the states.

The helper refuses a HOME or data directory outside the private evidence
fixture. Its offline contracts cover selected-view/placement cleanup, foreign-key
cascades, unchanged unrelated rows and monotonic clocks, and refusal to modify
an unrelated home or missing channel. A separate direct-SQL check exercises the
conditional trigger body only; it does not test daemon schema admission. Real
registry refusal and successful recovery must be checked in the native app:

```sh
python3 -I -B tests/fixtures/test_seed_refusal_registry.py -v
```
