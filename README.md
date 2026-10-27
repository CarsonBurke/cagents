<p align="center">
  <img src="packaging/dev.cagents.Cagents.svg" width="96" height="96" alt="">
</p>

<h1 align="center">cagents</h1>

<p align="center">A native Linux workspace for coding agents.</p>

Run Claude Code, Codex, and OMP across repositories and Git worktrees. Keep
conversations, terminals, files, diffs, and pull requests together, with a
separate workspace for each worktree.

A per-user daemon owns your terminals and conversations. Close the window or
restart the UI while your agents keep working.

## Work with your agents

- **Terminal or native chat.** Switch a supported agent session between a real
  terminal and a native chat view of the same conversation.
- **Parallel worktrees.** Create a worktree from a branch name or Linear issue.
  See which agents are working, waiting, or have unread output.
- **Review in place.** Read diffs, stage changes, commit, browse branch history,
  and review pull requests with GitHub's viewed marks kept in sync.
- **Keyboard navigation.** Move through a horizontal strip of chats, terminals,
  editors, and browsers. Resize panes without leaving the keyboard.
- **Built-in editor.** Edit files and working-tree diffs with syntax highlighting,
  autosave, and optional language servers. The editor also ships as `ceditor`.
- **Browser panes.** Open pages in the optional Servo browser. Browserless
  builds open links in your system browser.
- **Remote workspaces.** Connect to another machine's daemon over SSH for
  terminals, native chats, files, Git, and worktrees. See [device windows](docs/devices.md).
- **CLI automation.** Control projects, terminals, and channels with text or JSON
  output. [Submit prompts and read structured results](docs/agent-execution.md)
  without an open window, or let sessions message one another. Use
  `cagents linear` to work with tickets from the command line.

## Install

cagents runs on **Linux x86_64 with Wayland**, glibc 2.35 or newer, and Git 2.36
or newer. The release installer also requires an authenticated
[GitHub CLI](https://cli.github.com/) account (`gh auth login`).

Follow the [installation guide](docs/installation.md#install-with-the-verified-installer)
for the checksum-verified installer. It installs as your user without root.
You can also download full or browserless Debian packages and portable archives
from [Releases](https://github.com/CarsonBurke/cagents/releases).

Install the agent CLIs you want to use and sign in with your own accounts.

## Your first session

Launch **cagents** from your application menu, or run:

```sh
cagents ui
```

Add a repository in **Projects**, select a worktree, and press `Ctrl+Shift+T`
to open a terminal. Start your agent there, or choose **New tab → New thread**
for a native chat.

Choose **New tab → Resume conversation** to reopen a saved chat, including
conversations from configured provider history. Move between panes with
`Ctrl+PageUp` and `Ctrl+PageDown`; resize the focused pane with `Alt+R`.
The [user guide](docs/user-guide.md#navigation) covers the rest of the workspace
and its shortcuts.

To inspect the daemon and its terminals:

```sh
cagents daemon status
cagents --json terminal list
cagents --help
```

## Build from source

You need Rust 1.93, Zig 0.15.2, and the
[native build dependencies](docs/development.md#prerequisites).

```sh
git clone https://github.com/CarsonBurke/cagents.git
cd cagents
scripts/run-development --browserless
```

The development runner builds the matched CLI, UI, and daemon separately and
uses an isolated development instance that can run beside an installed copy.
Omit `--browserless` to include the embedded Servo browser.

See [development](docs/development.md) for build commands, tests, debugging,
and runner options.

## Documentation

- [Installation](docs/installation.md): packages, updates, and uninstalling
- [User guide](docs/user-guide.md): workspace, chat, review, editor, and shortcuts
- [Configuration](docs/configuration.md): settings and data locations
- [Device windows](docs/devices.md): work on remote machines over SSH
- [Agent execution](docs/agent-execution.md): hosted prompts, results, and messaging
- [Daemon and upgrades](docs/daemon.md): session ownership and upgrade behavior
- [Development](docs/development.md): building, running, and testing
- [Performance](docs/performance.md): workloads and reference results
- [Releasing](docs/releasing.md): building and publishing releases

## License

[MIT](LICENSE).
