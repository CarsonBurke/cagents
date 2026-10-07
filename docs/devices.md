# Device windows

A cagents window can connect to a daemon on another computer while the window
runs on your desktop. Terminals, native conversations, account probes, and the
**Device files** tools execute on the selected device. Each device has its own
saved windows; opening another device does not move or stop the existing jobs.

Install matching `cagents` and `cagentsd` executables on both computers. The remote
machine does not need a display or `cagents-ui`. Set up ordinary SSH access first:

```console
ssh fw16 hostname
```

SSH host aliases, ProxyCommand, Tailscale, keys, host verification, and agent
configuration remain under your SSH configuration. cagents uses noninteractive
SSH (`BatchMode=yes`); authenticate and accept the verified host key with SSH
before opening a device window. It does not expose a TCP daemon listener or copy
provider credential files to your desktop.

Use the current-device control at the bottom right of the status bar. Its menu
lists **This device** and your saved devices. Choose **Connect to a device…** to
enter an SSH host alias and remote executable, then **Save and switch device**.
The profiles are stored in `devices.json` beside `settings.toml` in the desktop's cagents configuration
directory (`~/.config/cagents/devices.json` on Linux; the development instance uses
its own configuration directory):

```json
{
  "devices": [
    {
      "name": "fw16",
      "host": "fw16",
      "executable": "/home/marvin/.local/bin/cagents"
    }
  ]
}
```

`executable` defaults to `cagents` and names the remote CLI, not a shell script
body. It is quoted as one executable name. Device names contain 1–16 letters,
digits, hyphens, or underscores. `local` is reserved for this computer.

```console
cagents ui
cagents --device fw16 ui
cagents --device fw16 ui --window work
cagents --device fw16 terminal list
cagents --device fw16 terminal spawn --cwd /home/marvin/Documents/repositories/cagents
```

Choosing a device from the status-bar menu switches the current client after the
new connection is ready. Its hosted jobs continue. Each device row also offers
**Open in another window** for keeping local and remote clients open together.
Named windows are scoped to their device; the internal `device-` namespace is reserved.

**Browse device files…** in the same menu browses remote directories, edits UTF-8 text files up to 512 KiB,
and runs Git status, unstaged/staged diff, stage, unstage, and commit on the device.
It can also list worktrees, create a branch and worktree under `.worktrees/`, and
remove a clean worktree without forcing deletion. `.worktrees/` must be ignored
in `.gitignore` or `.git/info/exclude` before creation.
**Use directory** selects the working directory for new remote terminals and
conversations and publishes the project to the remote daemon. Saves compare the
loaded content and serialize with other cagents saves; a changed file requires a
reload. File operations refuse symlink traversal outside their chosen directory.
Git subprocesses have output bounds and a 30-second timeout.

On Linux, an SSH login without `XDG_RUNTIME_DIR` uses the existing
`/run/user/<uid>` directory when it belongs to the current user and has mode
0700. Explicit runtime values keep the normal daemon validation. A headless
host without that login runtime directory must configure an owned private
`XDG_RUNTIME_DIR` for its daemon and SSH bridge command.

The connections of one window or command share authenticated SSH transports
through OpenSSH connection multiplexing, so only the first pays the SSH
handshake. cagents keeps these control sockets in its private runtime directory
and sets `ControlMaster`, `ControlPath`, and `ControlPersist` itself for those
connections, over any in your SSH configuration; your own SSH sessions and
control sockets are untouched. It stops the transports when the window or
command exits, and one left behind by a crash ends a minute after its last
stream. Each transport carries eight streams, below sshd's default
`MaxSessions` of 10; a lower server limit still works, with the refused streams
connecting separately. Where SSH cannot share a connection, each stream
connects separately under your own configuration, as before.

Closing the window detaches its SSH streams; daemon-hosted jobs continue. SSH
keepalives detect lost connections, and the normal terminal/chat reconnect paths
establish new streams. Startup failures identify the selected device and include
SSH diagnostics. A protocol mismatch requires installing a matched executable
set before reconnecting.

The first device-window implementation uses Device files for remote filesystem
and essential Git operations. The existing rich repository editor, language
servers, local history resume picker, pull-request UI, advanced worktree management, and
file attachments are not remote services yet; unsupported local operations are
refused in device windows and tracked in [#688](https://github.com/CarsonBurke/cagents/issues/688). Run advanced Git/worktree tools
in a remote terminal. Accounts, native provider sign-in, and managed account
creation, linking, registration, and removal belong to the selected device.
Codex uses device authentication so its callback need not be forwarded.
Per-pane device switching is tracked separately in [#687](https://github.com/CarsonBurke/cagents/issues/687).

Choose a device from the bottom-right device menu to switch the current window.
The replacement connects successfully before the current client closes; daemon
jobs continue. Save or discard editor changes before switching. Each device row
also has an **Open in another window** action, so a desktop window and an fw16
window can stay open together.

Clients on the same device and repository view (channel or worktree) share ordered pane IDs, titles, widths,
and exact terminal/chat sessions. Creating, moving or closing a pane updates the
other clients live. Focus, scrolling and unsent drafts belong to each window.
When another client removes a pane with local unsaved work, that work stays
available here until it is saved or discarded.
