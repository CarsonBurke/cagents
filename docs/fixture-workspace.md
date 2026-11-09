# Fixture workspaces

`scripts/fixture-workspace` fills a private directory, or a development
instance, with a workspace that looks like one a person has used for months:
projects with real Git worktrees, channels, open and long-closed cards, shared
views with years of recorded edits, and thousands of Claude Code and Codex
conversations in their providers' own on-disk formats. Use it to measure and
exercise the app at a realistic scale without touching anyone's real state.

It has two halves:

- **generate** (Python, standard library only) writes the repositories, the
  conversation history and `plan.json`, which describes the workspace.
- **apply** hands `plan.json` to development builds of `cagentsd` and
  `cagents-ui`, which load it through their own writers: the daemon applies
  `OpenProject`, `CreateChannel`, `SeedView` and `ChangeView` edits through its
  registry, and each window saves through its `StateStore`. Nothing in the
  stores is written by hand, so the stores are exactly what those versions of
  the app would have written.

```sh
root=$HOME/.local/state/fixture-workspace/realistic
scripts/fixture-workspace generate --profile realistic --root "$root"
scripts/fixture-workspace apply --root "$root" --bin-dir target/dev-runtime
```

Put roots under `~/.local/state`, not `/tmp` or `/dev/shm`, which are held in
memory: `realistic` is about 170 MB and `huge` about 1.1 GB.

## Profiles

| Profile | Shape | Generate |
| --- | --- | --- |
| `small` | 3 projects, 6 channels, 33 cards, 24 Claude and 8 Codex conversations. No edge cases. For tests. | 0.1 s, 2 MB |
| `realistic` | Measured from a long-lived installation: 18 projects, 76 channels, 1856 shared views (76 channel, 1780 worktree; 44 showing 83 panes), 45,970 recorded view edits with outliers of 36,608, 5,891 and 1,320, 1769 cards about 95% closed, a second window holding 1709 cards, 2400 Claude and 700 Codex conversations, every edge case `all` adds, and `large-record`. | 3.6 s, 170 MB |
| `huge` | Stress: 24 projects, 150 channels, 3538 cards, 10,000 Claude and 2,000 Codex conversations, every edge case `all` adds, and `large-record`. | 21 s, 1.1 GB |
| `sidebar` | The sidebar's resting states: 10 projects with avatars and initials, dirty worktrees, and unread counts of 3, 12 and 120. | 0.3 s, 6 MB |

Times are from one run on this repository's development host.

- `--seed N` chooses another workspace of the same shape. The same profile,
  seed and root write the same bytes, apart from Git's index (a stat cache)
  and checkout file times. History file times are fixed, since the resume
  picker orders by them.
- `--scale F` multiplies the counts (cards, conversations, edits, worktrees)
  and leaves the structure alone.
- `--edge-case NAME` adds one edge case, `--edge-case all` adds every one but
  the large ones (`large-record`, about 31 MiB; `large-transcripts`, about
  440 MiB; `oversized-transcript`, about 260 MiB), and `--no-edge-cases` drops
  the profile's. A large one is added only when named.
  `scripts/fixture-workspace edge-cases` lists them, with the profiles that
  include each.
- `--epoch SECONDS` moves "now"; every timestamp counts back from it,
  repository history included (default 2026-06-01).
- `--replace` regenerates a root generated before. It refuses a directory
  without the `.fixture-workspace` marker the generator writes first.

## What a root holds

```
ROOT/
  plan.json          the workspace, for apply and for tests to find things in
  projects/NAME/     Git repositories, with worktrees under .worktrees/
  home/.claude/      Claude Code projects/<slug>/<session>.jsonl and subagent sidecars
  home/.codex/       Codex sessions/YYYY/MM/DD/rollout-*.jsonl, archived_sessions/, session_index.jsonl
  avatars/           GitHub avatar images for projects with a github.com remote
  bin/               first on every generated chat and terminal's PATH; empty
```

After `apply` without `--instance`, and once the app runs on the root, it also
holds `data/`, `config/`, `cache/` and `state/`: the app's XDG directories for
this root.

Every chat and terminal card carries a launch environment that names `HOME`,
`CLAUDE_CONFIG_DIR` (`ROOT/home/.claude`), `CODEX_HOME` (`ROOT/home/.codex`)
and the four XDG base directories inside the root; the appliers refuse a plan
whose environment names any of them elsewhere or with `..`. The app layers a
recipe over its own inherited environment, so a recipe also removes what that
environment may carry to the person's accounts and desktop: the providers' API
keys and tokens, `GH_TOKEN` and the other forge tokens, the SSH agent,
askpass helpers, `DISPLAY`, `WAYLAND_DISPLAY` and the session bus. A provider
launched from a seeded card therefore reads the fixture's history and is
signed out.

The generator leaves `bin/` empty, so the provider a card launches is the first
`claude` or `codex` on the rest of the recipe's `PATH`
(`/usr/local/bin:/usr/bin:/bin`); native-evidence puts its synthetic Claude in
`bin/`. When a window opens, it reconnects every open chat card in every
channel, not only those on screen, as it does for any workspace. A seeded
daemon has hosted none of them, so none reconnects:

- a Claude chat shows "Saved Claude history could not be opened without
  starting a provider" and starts nothing;
- a Codex chat that has a session asks for a Codex app-server under the
  fixture's `CODEX_HOME`. In native-evidence, with no `codex` on the recipe's
  `PATH`, it shows "Cannot join existing Codex daemon" and no `codex` runs.
  The app sends a start for each such card, so with `codex` on that `PATH`
  expect one signed-out `codex` per open Codex card, confined to the fixture.

Signing in or switching accounts from a seeded window is not confined: the
account flow resolves the provider's home from the window's own environment,
which under `--instance` is your real one. Do not sign in from a seeded
instance.

The conversations follow the providers' current formats, including what
makes real transcripts hard to read: harness noise (system reminders, local
command output, compaction summaries, task notifications, skill bodies),
long code-heavy replies, tool calls with results, synchronous and background
subagents with their sidecar files, titles, and Codex harness context
(`AGENTS.md`, environment context) before the first prompt. Codex subagent
threads carry the `thread_spawn` source codex 0.166 writes. Damage follows
what Claude Code 2.1.296 was seen to write: a fork (`--resume ID
--fork-session`) is a new file starting with its origin's messages, with their
uuids and working directory but the fork's own sessionId; a crash mid-save
cuts a message short and appends the next one to the same line, following on
from the message before the cut one.

`plan.json` names the file each edge case went into (`edge_cases`), each
conversation's file, visible message count and turns (`history`), and the
Codex threads a live writer would lock (`history.writer_locks`).

## Loading a workspace

`apply` runs the daemon first, which writes the shared workspace model and
reports its identity, then each window in the plan with that identity, so the
windows know the registry already holds their channels.

Without `--instance`, every store goes inside the root, and each run takes its
leases in a private runtime directory there that is removed afterwards:

```sh
scripts/fixture-workspace apply --root "$root" --bin-dir target/dev-runtime
```

The seeding flags (`cagentsd --development-seed-workspace PLAN`,
`cagents-ui --development-seed-workspace PLAN --window SLOT`) exist only in
development builds; a production executable refuses them. Each seed also
refuses:

- while this instance's daemon runs, or the window it seeds is open. Each
  seed holds the daemon's lease while it writes, the window seed too, and the
  window's own lease. Without `--instance` those leases are the run's own, so
  they cannot see a daemon or window started on the root's directories by
  hand; the next point catches one, since an open store has its WAL beside it;
- an instance whose data directory differs in any way from what earlier seeds
  of the same plan left there. The seeds record every entry of that directory,
  at any depth, with its size and modification time, in `fixture-seed.json`
  there, and each seed compares the directory with it: anything added, changed
  or gone means the instance was used, and a seed never merges into real
  state. This is how the daemon and each window seed one after another;
- an instance another plan seeded, or the same path since generated again
  (`generate --replace`): the ledger names the plan by its path and the SHA-256
  of its bytes;
- an instance another seed is writing: each seed locks the data directory;
- the unnamed development instance, the login daemon's, unless every XDG
  directory it would write, and `XDG_RUNTIME_DIR` where it takes its leases,
  is inside the plan's root, as `apply` without `--instance` arranges;
- a plan in another format, one moved from where it was generated, whose
  paths would name the wrong repositories, or one whose avatar owners are not
  GitHub logins.

A store is written beside its final name, renamed into place once closed and
synced, and kept only once the ledger, synced too, records it. A seed that
fails removes what it wrote, avatars included; its directories above the
store's own stay. A seed killed between the rename and the record leaves a
store the ledger does not name, which every later seed of that instance
refuses until its data directory is deleted.

### A development instance

To browse a seeded workspace in the native app, seed an instance that has
never been launched, then start it. Build the two executables the seed needs
as the runner does, one feature set per invocation:

```sh
scripts/cargo-gate build --locked --profile dev-runtime --no-default-features \
    --features development,host --bin cagentsd
scripts/cargo-gate build --locked --profile dev-runtime --no-default-features \
    --features development,ui --bin cagents-ui
scripts/fixture-workspace apply --root "$root" --bin-dir target/dev-runtime --instance seeded
scripts/run-development --browserless --instance seeded
```

`--bin-dir` is `dev-runtime` under the checkout's Cargo target directory,
`target/` unless `CARGO_TARGET_DIR` or the Cargo configuration moves it. To
seed an instance again, stop it with `--stop-instance seeded` and delete its
data directory (`~/.local/share/cagents-development-seeded`) first, and its
cache (`~/.cache/cagents-development-seeded`) for fresh avatars; a seed refuses
an instance that has been used. The instance runs with your real `HOME`, so
its own windows and account flow use your real provider homes, but every
seeded card launches with the fixture's. `apply` refuses the unnamed
development instance, which is the login daemon's.

### A native-evidence fixture

`scripts/native-evidence seed` generates into the task's fixture, applies it
through a built side's executables and saves it as the seed every `start`
restores:

```sh
scripts/native-evidence build after .
scripts/native-evidence seed --profile realistic --seed 1
scripts/native-evidence start after
```

It uses the side's own checkout's generator, which matches that side's
executables; `--side before` uses the other one. A before checkout from
before this tool existed cannot seed, and the stores an after side writes are
only readable by a before side at the same or an older schema.

## What a seed cannot show

Seeded state is what the app saved, so anything the app only learns from a
live process is absent:

| Static, seeded | Needs a live provider or daemon |
| --- | --- |
| Projects with avatars (cached, from a `github.com` remote) or initials | A working agent's spinner |
| Dirty worktrees | An agent waiting for attention |
| Unread counts per worktree and project, including the `99+` cap | A failed update, which comes from the daemon's journal |
| Open, closed and retired cards; channel order and titles; drafts | Terminal scrollback and running processes |
| Conversation history the resume reader reads from the providers' files | A seeded chat card's history when a window opens, which needs a daemon that hosted it |

The Claude transcripts read back through the app's resume reader with the
messages `plan.json` counts, which the Rust tests check for every intact one.
A seeded chat card does not show its history when a window opens (see the
launch environment above). `check-codex` below verifies the rollouts a real
`codex` reads.

## Other commands

- `check-codex --root ROOT` reads every rollout back through a real
  `codex app-server` (`thread/read` with turns) and compares the turn counts.
  It is skipped when `codex` is not installed, and runs with only `PATH`,
  `HOME`, `CODEX_HOME` and `LANG` set.
- `hold-writer-locks --root ROOT [--thread ID]...` holds the Codex writer
  locks `plan.json` names, as a live `codex` writing those threads would,
  until interrupted.

## Changing the generator

- A knob goes in `Profile` (`scripts/fixture_workspace/profiles.py`) with a
  default, is read where the workspace (`workspace.py`) or history
  (`claude.py`, `codex.py`) is built, and gets a test in
  `tests/fixture_workspace.py`.
- An edge case gets a name and description in `EDGE_CASES`, is written exactly
  once, and is recorded in `plan.json`'s `edge_cases`, so a test can find it.
- A change the Rust appliers must understand bumps `PLAN_FORMAT` in
  `scripts/fixture_workspace/generate.py` and `src/fixture_workspace.rs` in
  the same commit.
- Streams are named (`Streams(seed)`), so adding randomness to one part of the
  plan leaves the rest of a seed's workspace as it was.

Tests: `python3 -I -B tests/fixture_workspace.py -v` for the generator, and
the round trips through the real readers, which generate `small` and load it:

```sh
scripts/cargo-gate test --locked --no-default-features --features host --lib fixture
scripts/cargo-gate test --locked --no-default-features --features ui --lib fixture
```
