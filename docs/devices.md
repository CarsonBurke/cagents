# Device windows

A cagents window can connect to a daemon on another computer while the window
runs on your desktop. Terminals, native conversations, account probes, and the
**Device files** tools execute on the selected device. Each device has its own
saved windows; opening another device does not move or stop the existing jobs.

Install matching `cagents` and `cagentsd` executables on both computers. The remote
machine does not need a display or `cagents-ui`. Set up ordinary SSH access first:

```console
ssh build-host hostname
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
      "name": "build-host",
      "host": "build-host",
      "executable": "/home/user/.local/bin/cagents"
    }
  ]
}
```

`executable` defaults to `cagents` and names the remote CLI, not a shell script
body. It is quoted as one executable name. Device names contain 1–16 letters,
digits, hyphens, or underscores. `local` is reserved for this computer. A saved
name keeps its connection: saving the same name with another host or executable
is refused. The file holds at most 64 devices and 64 KiB, and a save that would
exceed either is refused. Windows saving at the same time each keep their device.

```console
cagents ui
cagents --device build-host ui
cagents --device build-host ui --window work
cagents --device build-host daemon status
cagents --device build-host terminal list
cagents --device build-host terminal spawn --cwd /home/user/projects/example
```

`daemon status` and `daemon stop` never start the device's daemon; status
reports a stopped one as stopped. Other commands start it unless you pass
`--no-start`.

Choosing a device from the status-bar menu switches the current client after the
new connection is ready. Its hosted jobs continue. Each device row also offers
**Open in another window** for keeping local and remote clients open together.
Named windows are scoped to their device; the internal `device-` namespace is reserved.

**Browse device files…** in the same menu browses remote directories, edits UTF-8 text files up to 512 KiB,
and runs Git status, unstaged/staged diff, stage, unstage, and commit on the device.
It can also list worktrees, create a branch and worktree under `.worktrees/`, and
remove a clean worktree without forcing deletion. Creation adds a clone-local
`/.worktrees/` rule to Git's common `info/exclude` when needed, matching local
creation without editing tracked `.gitignore`.
**Use directory** selects the working directory for new remote terminals and
conversations and publishes the project to the remote daemon. Saves compare the
loaded content and serialize with other cagents saves; a changed file requires a
reload. If the replacement is published but syncing its directory fails, the
editor reports that the file was saved with unconfirmed durability. The new
content becomes the saved baseline; the warning means crash durability could
not be confirmed. File operations refuse symlink traversal outside their chosen
directory.
Git subprocesses have output bounds and a 30-second timeout.

On Linux, an SSH login without `XDG_RUNTIME_DIR` uses the existing
`/run/user/<uid>` directory when it belongs to the current user and has mode
0700. Explicit runtime values keep the normal daemon validation. A headless
host without that login runtime directory must configure an owned private
`XDG_RUNTIME_DIR` for its daemon and SSH bridge command.

A phone runs `cagents daemon bridge --mobile` instead, which speaks the
stable, versioned [mobile API](mobile-api.md) rather than the daemon's
internal protocol.

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
also has an **Open in another window** action, so a desktop window and a build-host
window can stay open together.

Clients on the same device and repository view (channel or worktree) share ordered pane IDs, titles, widths,
and exact terminal/chat sessions. Creating, moving or closing a pane updates the
other clients live. Focus, scrolling and unsent drafts belong to each window.
When another client removes a pane with local unsaved work, that work stays
available here until it is saved or discarded.

## Agents on a device

An agent running in cagents on this computer can start and drive agents on a
saved device, once you allow it. Link the device and allow agents from a system
terminal outside cagents:

```console
cagents device link build-host --allow-agents
```

Then a session on this computer uses the device as you would:

```console
cagents --device build-host agent spawn --provider codex --project /home/user/projects/example 'Run the tests'
cagents --device build-host agent wait AGENT
cagents --device build-host agent send AGENT 'Fix the failure too'
cagents --device build-host agent close AGENT
```

On the device, the session has a session's scope: it may start, message, wait
for and close its own agents and terminals there, and message other sessions.
Like a session on the device, it can also read every terminal's screen and
every conversation there, including your own, so allow agents only on a device
whose contents you would show them. Through cagents it may not change accounts,
the daemon, installations or device links, and, as on this computer, it may
only use a project already open on the device. Its agents there are its
children: they belong to the session that started them, not to the device's own
sessions. It is held to the permission ceiling it has here, as of its
connection's latest proof: the agents it starts on the device with
`agent spawn` get no more than it could give them here, and it may approve
their tool requests there only if it could here. A terminal it starts with
`terminal spawn` runs any command on the device as you, and, as on this
computer, the permission ceiling does not
cover a provider CLI it starts that way. Allowing agents on a device therefore
lets sessions here run commands there as you. An agent it starts on the device
is a session of that device, so it may use the device's own links to further
devices if a person there allowed them.

| Command | What it does |
| --- | --- |
| `cagents device list` | Saved devices, whether each is linked, and whether agents here may use it. |
| `cagents device link NAME [--allow-agents]` | Links a saved device. Linking again replaces its key; the agents sessions here started on the device stay theirs. |
| `cagents device allow-agents NAME` | Lets sessions on this computer use a linked device. |
| `cagents device deny-agents NAME` | Stops them. Agents they already started there keep running. |
| `cagents device unlink NAME` | Forgets the link here and on the device. |

Only a person may link, allow, deny or unlink: the CLI refuses these commands
in a cagents session, so an agent cannot grant itself a device through cagents.
Like every session scope, this is cagents' own interface, not an
operating-system sandbox: an agent with a shell and your SSH keys can already
act as you on the device. A session that tries a device it may not use is told
so, with the command a person can run, rather than a connection error.

Linking stores a random key in `device-trust.json`, readable only by you, in
each daemon's data directory. A session's own capability never leaves this
computer, and SSH never carries it. For each connection, the device asks for a
fresh challenge to be signed; this computer's daemon signs it only for a live
session and only while agents are allowed. The signature covers the session's
lineage and its permission ceiling, so nothing relaying it can raise either.
The key guards against relaying, not against a process running as you, which
can read it. The device then issues a capability for that connection, which
lapses after 30 seconds unless the connection proves itself again; `cagents`
does so before its next request once 15 seconds have passed, so a command that
polls, such as `agent wait`, keeps its connection. Each time, this computer's
daemon vouches for the session as it is then, so denying agents, unlinking,
ending the session or changing its mode reaches a connection that is already
open within 30 seconds: the device closes it, or holds it to the session's new
permission ceiling. A request the device already accepted still finishes, and
agents the session started there keep running with the ceiling they started
with. When `unlink` reaches the device, the device
closes that computer's connections at once, as it does when a device is linked
again with a new key. Unlinking deletes the key here at once, so no connection
can be proved again even when the device cannot be reached to forget it.
