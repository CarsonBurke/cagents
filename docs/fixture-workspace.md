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
| `realistic` | Measured from a long-lived installation: 18 projects, 76 channels, 1856 shared views (76 channel, 1780 worktree; 44 showing 83 panes), 45,970 recorded view edits with outliers of 36,608, 5,891 and 1,320, 1769 cards about 95% closed, a second window holding 1709 cards, 2400 Claude and 700 Codex conversations, and every edge case. | 3.6 s, 170 MB |
| `huge` | Stress: 24 projects, 150 channels, 3538 cards, 10,000 Claude and 2,000 Codex conversations, every edge case. | 21 s, 1.1 GB |
| `sidebar` | The sidebar's resting states: 10 projects with avatars and initials, dirty worktrees, and unread counts of 3, 12 and 120. | 0.3 s, 6 MB |

Times are from one run on this repository's development host.

- `--seed N` chooses another workspace of the same shape. The same profile,
  seed and root write the same bytes, apart from Git's index (a stat cache)
  and checkout file times. History file times are fixed, since the resume
  picker orders by them.
- `--scale F` multiplies the counts (cards, conversations, edits, worktrees)
  and leaves the structure alone.
- `--edge-case NAME` adds one edge case, `--edge-case all` adds every one but
  `oversized-transcript`, and `--no-edge-cases` drops the profile's.
  `scripts/fixture-workspace edge-cases` lists them, with the profiles that
  include each.
- `--epoch SECONDS` moves "now"; every timestamp counts back from it
  (default 2026-06-01).
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
  bin/               on every generated chat and terminal's PATH; empty
```

After `apply` without `--instance`, it also holds `data/`, `config/`,
`cache/` and `state/`: the app's XDG directories for this root.

Every chat and terminal card carries a launch environment with `HOME` set to
`ROOT/home` and the XDG directories inside the root, so a provider launched
from a seeded card reads the fixture's history, never `~/.claude` or
`~/.codex`.

The conversations follow the providers' current formats, including what
makes real transcripts hard to read: harness noise (system reminders, local
command output, compaction summaries, task notifications, skill bodies),
long code-heavy replies, tool calls with results, synchronous and background
subagents with their sidecar files, titles, and Codex harness context
(`AGENTS.md`, environment context) before the first prompt. Codex subagent
threads carry the `thread_spawn` source codex 0.166 writes. A fork is filed
as codex and Claude Code do: a new file starting with its origin's records
under the origin's session and working directory.

`plan.json` names the file each edge case went into (`edge_cases`), each
conversation's file, visible message count and turns (`history`), and the
Codex threads a live writer would lock (`history.writer_locks`).

## Loading a workspace

`apply` runs the daemon first, which writes the shared workspace model and
reports its identity, then each window in the plan with that identity, so the
windows know the registry already holds their channels.

Without `--instance`, every store goes inside the root with a private runtime
directory that is removed afterwards, and no running daemon is involved:

```sh
scripts/fixture-workspace apply --root "$root" --bin-dir target/dev-runtime
```

The seeding flags (`cagentsd --development-seed-workspace PLAN`,
`cagents-ui --development-seed-workspace PLAN --window SLOT`) exist only in
development builds; a production executable refuses them. Each seed also
refuses:

- while a daemon or window holds this instance's lease;
- a store that already exists, naming its schema: a seed never merges into
  real state;
- a plan in another format, or one moved from where it was generated, whose
  paths would name the wrong repositories.

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
seed an instance again, stop it with `--stop-instance seeded` and delete
its storage (`~/.local/share/cagents-development-seeded` and its config and
state siblings) first; a seed refuses stores that exist. The instance keeps
your real `HOME` for provider sign-ins, but every seeded card launches with
the fixture's `HOME`. `apply` refuses the unnamed development instance, which
is the login daemon's.

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
| Conversation history for resume and read-only history | Codex history in the app, which a `codex` binary serves |

A seeded chat opens its conversation's history from the fixture's files. A
Codex chat needs `codex` on `PATH` to show it; `check-codex` below verifies the
rollouts a real one reads.

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
