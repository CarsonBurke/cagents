<p align="center">
  <img src="packaging/dev.cagents.Cagents.svg" width="96" height="96" alt="">
</p>

<h1 align="center">cagents</h1>

<p align="center">A native Linux workspace for running coding agents across repositories and Git worktrees.</p>

cagents keeps every agent session, terminal, and worktree in one native
Wayland application. Run Claude Code, Codex, and OMP in real terminals or
native chat views, and review their changes and pull requests next to them. A
per-user daemon owns the terminals and conversations, so they keep running when
you close the window or restart the UI.

> [!IMPORTANT]
> The repository is private (`CarsonBurke/cagents`). Downloading a release
> needs a GitHub CLI account with access to it.

## Features

- **Projects and worktrees.** Open repositories, create worktrees from a branch
  name or Linear issue, and see which worktrees have working, waiting, or
  unread agents at a glance.
- **Persistent terminals.** Terminals and chats belong to the `cagentsd`
  daemon, so they survive UI restarts, and supported sessions resume across
  full upgrades.
- **Native agent chat.** Switch a Claude, Codex, or OMP terminal into a native
  chat view of the same conversation, and back.
- **Keyboard-driven panes.** Each worktree is a horizontal strip of terminals,
  chats, editors, and browsers you move through and resize from the keyboard.
- **Source control and review.** Diffs, staging, commits, branch history, and a
  pull request pane that syncs GitHub's viewed marks.
- **Built-in editor.** Autosaving editor with syntax highlighting, editable
  working-tree diffs, and optional language servers. It also ships separately
  as `ceditor`.
- **Embedded browser.** An optional Servo browser pane. The browserless build
  opens links in your system browser instead.
- **Scriptable CLI.** `cagents` controls the daemon, terminals, projects, and
  channels with human-readable or JSON output. [Hosted agent execution](docs/agent-execution.md)
  submits durable prompts and reads structured results without an open window,
  and lets sessions message one another.
- **Linear CLI.** Read, search, create, and update tickets with `cagents linear`,
  attach PR links, and link each worktree's ticket for `--current` commands.
  Uses a personal API key and works without an open window.

## Install

Requirements: Linux x86_64 with glibc 2.35 or newer, a Wayland session, Git
2.36 or newer, and an authenticated [GitHub CLI](https://cli.github.com/)
(`gh auth login`) that supports `gh auth status --json hosts`.

[docs/installation.md](docs/installation.md) has the recommended
checksum-verified installer, a shorter convenience installer, the full and
browserless Debian packages, the portable archive, and how to update and
uninstall. The installer runs as your user and never needs root.

## Getting started

Launch **cagents** from your application menu, or run:

```sh
cagents ui
```

Add a repository from the **Projects** pane, select a worktree, and press
`Ctrl+Shift+T` for a terminal. Start your agent there, or use **New tab → New
thread** for a native chat. Use **New tab → Resume conversation** to search saved
native chats and configured provider history, including closed conversations from
previous runs. Move between panes with `Ctrl+PageUp` and
`Ctrl+PageDown`, and resize the focused pane with `Alt+R`. The
[user guide](docs/user-guide.md#navigation) lists every shortcut.

The daemon is scriptable too:

```sh
cagents daemon status
cagents --json terminal list
cagents --help
```

## Run from source

You need Rust 1.93, Zig 0.15.2, and the native build packages listed in
[docs/development.md](docs/development.md#prerequisites).

Run the app with the development runner. It builds the matched `cagents`,
`cagents-ui`, and `cagentsd` executables with separate feature sets and uses an
isolated development instance, so it runs beside an installed copy:

```sh
scripts/run-development --browserless   # without the embedded browser (faster to build)
scripts/run-development                 # with the embedded Servo browser
```

Useful options:

```sh
# Debug logs and backtraces, written to a file
scripts/run-development --browserless --debug --log-file /tmp/cagents-ui.log

# Restart the development daemon after changing daemon, terminal-host, or IPC code.
# This terminates the development daemon's terminals.
scripts/run-development --browserless --fresh-daemon
```

[docs/development.md](docs/development.md) covers the per-boundary
`cargo check` commands, every runner option, test lanes, build memory use, and
troubleshooting.

## Documentation

- [Installation](docs/installation.md): requirements, verified install,
  packages, updates, uninstalling
- [User guide](docs/user-guide.md): workspace, navigation, review, chat,
  editor, and `ceditor`
- [Configuration](docs/configuration.md): data locations and every
  `settings.toml` key
- [Device windows](docs/devices.md): SSH connections to remote daemons, remote
  terminals, native chats, files, Git, and worktrees
- [Daemon and upgrades](docs/daemon.md): the daemon, control CLI, and how
  sessions survive upgrades
- [Development](docs/development.md): building, running, and testing from
  source
- [Performance](docs/performance.md): latency and throughput workloads and
  reference results
- [Releasing](docs/releasing.md): building and publishing release artifacts

## License

cagents is licensed under the MIT License. See [LICENSE](LICENSE).
