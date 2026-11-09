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
| GitHub availability, project avatars or pull request rows in the sidebar, offline | The fixture's forge, [`tests/fixtures/fixture-gh.sh`](#an-offline-github), switched on by `home/.config/fixture-forge` |
| A Codex app-server peer that plays scripted items | `tests/fixtures/terminal-escape-codex.py` and the peers built on its transport, such as `codex-tool-rows.py` |
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
   fails until [switched on](#an-offline-github), and a `curl` that always
   fails. `save-seed` keeps it as the data every later start restores,
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
`$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge` exists; it reads the
switch on every call, so a step can turn it on in a running session. Then it
answers as a gh signed in to github.com as `fixture`, whose repositories have
no pull requests and no review requests:

| The app runs | It answers |
| --- | --- |
| `gh auth status --json hosts`, with or without `--hostname` | a successful `fixture` account for github.com; `{"hosts":{}}` for any other host |
| `gh repo view --json nameWithOwner,url` | the repository of the checkout's first github.com remote, in gh's order (`upstream`, `github`, `origin`, then the rest), or gh's own error when there is none |
| `gh pr list ... --json FIELDS` | `[]`, or the seeded pull requests below |
| `gh pr view N` | a seeded pull request, or gh's error for an unknown number |
| `gh pr checks N` | gh's `no checks reported` for a seeded pull request, which the app reads as no checks |
| `gh api graphql` | empty pages for the review-request poll, the pull request browser and its search; not found for a pull request's own queries |
| `gh api repos/OWNER/NAME/releases` | `[]`; any other REST endpoint is `404 Not Found` |

Anything else, including every write, fails and names the command. A project
shows as connected to GitHub only when its checkout has a github.com remote,
so add one: `git remote add origin https://github.com/OWNER/NAME`. The fixture
sets `GIT_ALLOW_PROTOCOL=file`, so Git never fetches from or pushes to it.

Avatars never come from the network: the fixture's `curl` always fails. Put an
image at `$CAGENTS_EVIDENCE_FIXTURE/avatars/OWNER.image`, PNG, JPEG, GIF or
WebP, at most 1 MiB and 1024×1024 pixels. Each time the app resolves a
repository, the forge copies every seed into the app's avatar cache,
`$XDG_CACHE_HOME/cagents-development/github-avatars/github.com-OWNER.image`
with the owner in lower case, and gives it a fresh modification time, so the
app reads it instead of downloading. Write the seed first, as below. The app
loads each owner's avatar once, as soon as an open project has both the remote
and the switch, and remembers a missing one until it restarts.

For pull request rows, `home/.config/fixture-forge-prs.json` holds an array in
the shape `gh pr list --json` prints; copy the fields of `PR_VIEW_FIELDS` in
`src/github.rs` and the examples in its tests. Each repository lists the
entries whose `url` is under `https://github.com/OWNER/NAME/pull/`, filtered by
`--state`, `--head`, `--author` and `--limit`. Reading the file needs `jq`.
The pull request browser still lists none of them. Opening one's review needs
its conversation queries, which the forge answers as not found; like any failed
GitHub read, that pauses the app's GitHub refreshes for a while.

A steps file for a sidebar with an avatar:

```sh
evidence env sh -c 'magick -size 64x64 gradient:"#2f6feb"-"#a371f7" "png:$CAGENTS_EVIDENCE_FIXTURE/avatars/octo-org.image"'
evidence env git -C '{fixture}/project' remote add origin https://github.com/octo-org/fixture-app
evidence env sh -c 'touch "$CAGENTS_EVIDENCE_FIXTURE/home/.config/fixture-forge"'
evidence cli project open '{fixture}/project'
# The app reads GitHub in the background; give it time before the shot.
sleep 12
evidence shot sidebar-avatar
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
host. `pair` can wait 30 minutes for the native lock, so run it in the
background rather than under a short command timeout, and run `clean` once the
PR merges to remove the task's checkouts from the host.

## When a fixture fails

Every refusal names its cause and the next command. The common ones:

| Message | Cause and fix |
| --- | --- |
| `requires a genuine User shell` (exit 77) | You are in a cagents session. Add `--remote fw16`. |
| `task directory must be outside Git ancestry` | Recovery storage refuses to live in a checkout. Use `~/.local/state/cev/NAME`. |
| `changed since its build; run: build SIDE CHECKOUT` | Evidence must show what was built. Build again. |
| `has not been built, or its unused build was reclaimed` | `scripts/prune-build-dirs` removed an idle target. Build again; a shared slot recompiles only what changed. |
| `refusing to compile ... while holding the native lock` | Something in the session asked Cargo for a build that `build` didn't do. Build first; keep builds out of a session. |
| `the native lock stayed busy` | Another session is running. `scripts/lock-holders /var/tmp/cagents-fw16-native.lock` shows whose. |
| `memory-capped: stopped` | The session exceeded its memory limit; the log names the process. |
| `session: lost` in `status` | Its supervisor was killed. `stop` ends what remains. |
| `the SIDE side is running on HOST; stop it before rebuilding` | A side's checkout can't change under its session. Run the `stop` it names, then build. |
| `task NAME on HOST belongs to OWNER` | Another agent's task has that name on the host. Give your task directory another name, such as your PR's. |
| `... is not a task this route started` | A task of that name exists on the host without an owner: someone runs it there directly, or an earlier version of the route started it. If it is yours, run the command the message gives on the host; otherwise rename yours. |
| `this task directory is the host's own` | You are on the build host. Run `scripts/native-evidence` without `--remote`. |

If a fixture fails in a way that doesn't point to the fix, that is a fixture
bug: make the message say what is wrong and what to do, and add a test for it
beside the others in the fixture's contract suite.
