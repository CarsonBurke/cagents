<p align="center">
  <img src="packaging/dev.cagents.Cagents.svg" width="96" height="96" alt="">
</p>

<h1 align="center">cagents</h1>

<p align="center">A native Linux workspace for running coding agents across repositories and Git worktrees.</p>

cagents keeps every agent session, terminal, and worktree in one fast Wayland
application. Run Claude Code, Codex, and OMP in real terminals or native chat
views, review their changes and pull requests next to them, and close the
window without losing anything: a per-user daemon keeps terminals and
conversations running in the background.

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
- **Scriptable CLI.** `cagents` controls the daemon and terminals with
  human-readable or JSON output.

## Install

Requirements: Linux x86_64 with glibc 2.35 or newer, a Wayland session, Git
2.36 or newer, and an authenticated [GitHub CLI](https://cli.github.com/)
(`gh auth login`).

Install the latest release for your user (no root needed):

```sh
(
  gh auth status --hostname github.com || exit 1
  tag="$(gh release view --repo github.com/CarsonBurke/cagents --json tagName --jq .tagName)" || exit 1
  case "$tag" in v*) ;; *) exit 1 ;; esac
  download_dir="$(mktemp -d)" || exit 1
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  gh release download "$tag" --repo github.com/CarsonBurke/cagents \
    --pattern install.sh --dir "$download_dir" &&
    sh "$download_dir/install.sh" --version "${tag#v}"
)
```

This runs the installer without checking it first. For a checksum-verified
install, Debian packages (full and browserless), the portable archive,
updates, and uninstalling, see [docs/installation.md](docs/installation.md).

## Getting started

Launch **cagents** from your application menu, or run:

```sh
cagents ui
```

Add a repository from the **Projects** pane, select a worktree, and press
`Ctrl+T` for a terminal. Start your agent there, or use **New tab → New
thread** for a native chat.

| Action | Default shortcut |
| --- | --- |
| New terminal | `Ctrl+T` |
| New channel (project-wide chat) | `Alt+G` |
| Clone focused pane | `Alt+E` |
| Close focused column | `Ctrl+W` |
| Search projects | `Ctrl+F` |
| Previous / next column | `Alt+Q` / `Alt+W` (or `Alt+Left` / `Alt+Right`) |
| Cycle pane width | `Alt+R` |
| Previous / next workspace | `Alt+Shift+Tab` / `Alt+Tab` |
| Review: mark viewed, next, previous file | `Alt+V`, `Alt+Down`, `Alt+Up` |

Shortcuts are configurable in Settings or in
[`settings.toml`](docs/configuration.md#keyboard-shortcuts).

The daemon is also scriptable:

```sh
cagents daemon status
cagents terminal list
cagents --json terminal list
cagents --help
```

## Run from source

You need Rust 1.93, Zig 0.15.2, and the native build dependencies. On Ubuntu
22.04:

```sh
sudo apt-get update
sudo apt-get install --yes --no-install-recommends \
  build-essential clang cmake git libclang-dev libdbus-1-dev \
  libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev libgbm-dev \
  libgl1-mesa-dev libssl-dev libudev-dev libwayland-dev \
  libxkbcommon-dev ninja-build pkg-config python3 xdg-utils xz-utils
rustup toolchain install 1.93.0 --profile minimal
rustup default 1.93.0
```

Install Zig 0.15.2 from [ziglang.org](https://ziglang.org/download/) (or run
`mise install`) and put `zig` on `PATH`.

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

Check the boundary you changed instead of building everything:

```sh
cargo check --locked --no-default-features --bin cagents                          # CLI
cargo check --locked --no-default-features --features host --bin cagentsd         # daemon
cargo check --locked --no-default-features --features ui --bin cagents-ui        # browserless UI
cargo check --locked --features integrated-browser --bin cagents-ui               # integrated UI
```

Builds use two compilation jobs by default; set `CARGO_BUILD_JOBS=1` on a
memory-constrained machine. [docs/development.md](docs/development.md) covers
the feature boundaries, every runner option, test lanes, resource usage, and
troubleshooting.

## Documentation

- [Installation](docs/installation.md): requirements, verified install,
  packages, updates, uninstalling
- [User guide](docs/user-guide.md): workspace, navigation, review, chat,
  editor, and `ceditor`
- [Configuration](docs/configuration.md): data locations and every
  `settings.toml` key
- [Daemon and upgrades](docs/daemon.md): the daemon, control CLI, and how
  sessions survive upgrades
- [Development](docs/development.md): building, running, and testing from
  source
- [Performance](docs/performance.md): latency and throughput workloads and
  reference results
- [Releasing](docs/releasing.md): building and publishing release artifacts

## License

cagents is licensed under the MIT License. See [LICENSE](LICENSE).
