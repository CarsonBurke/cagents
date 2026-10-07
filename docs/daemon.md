# Daemon and upgrades

- [How the daemon runs](#how-the-daemon-runs)
- [Control CLI](#control-cli)
- [Compatible upgrades](#compatible-upgrades)
- [Full daemon upgrades](#full-daemon-upgrades)
- [Recovering after a daemon change](#recovering-after-a-daemon-change)
- [Debian packages](#debian-packages)
- [Development instance](#development-instance)

## How the daemon runs

Terminals and native agent chats are owned by `cagentsd`, a per-user daemon
that runs independently of the UI so sessions survive window and UI-process
restarts. There is no system daemon or service unit. The standard production
installation starts the daemon immediately and registers it for
graphical-login autostart. While the daemon runs, cagents shows a system-tray
item that opens the UI and can stop the daemon. Its menu leads with the
daemon's state, or an action's result for a few seconds, and how many hosted
agents are working or waiting for you. Stopping a daemon that still hosts
terminals or agent work asks for a counted confirmation first.

Full and browserless production builds share one daemon socket
(`$XDG_RUNTIME_DIR/cagents/`) and one session store. IPC protocol changes
deliberately keep that socket namespace, so a newer daemon cannot start as a
second owner while an incompatible older daemon is still running. The
handshake rejects incompatible clients without terminating their PTYs.
Upgrade the three companion executables (`cagents`, `cagents-ui`, `cagentsd`)
together.

Each window holds an exclusive slot lease and its own session database, and a
shared compatibility fence excludes older single-window UIs. Startup rejects an
older running UI or Debian peer package that predates the lease marker; upgrade
that peer before switching variants.

Where the daemon keeps its state is listed in
[Configuration](configuration.md#files-and-directories).

## Control CLI

The `cagents` CLI prints human-readable output by default and stable JSON
envelopes with `--json`. Run `cagents --help`, or `--help` on any subcommand,
for the complete reference.

```sh
cagents daemon status
cagents terminal list
cagents terminal spawn -- bash -lc 'printf "hello from cagents\\n"'
cagents --json terminal list
cagents daemon stop --require-empty
cagents daemon stop --confirm
```

| Command | Purpose |
| --- | --- |
| `ui [--new-window \| --window NAME]` | Launch the UI, open another window, or open a named window |
| `daemon start` | Start the daemon if it is not already running |
| `daemon status` | Report whether the daemon runs, with `cli_version` and `daemon_version` |
| `daemon upgrade [--yes] [--interrupt]` | Review a staged installation, save windows, replace the service when needed, and wait for completion |
| `daemon stop` | Request daemon termination (see below) |
| `daemon preflight` | Print a short-lived shutdown capability for `daemon stop --confirm` |
| `daemon prepare-upgrade --yes` | Checkpoint resumable sessions and stop for a full upgrade |
| `daemon abandon-upgrade --yes` | Archive an interrupted upgrade checkpoint without resuming it |
| `daemon protocol-version` | Print this executable's wire protocol version without contacting a daemon |
| `terminal list`, `spawn`, `write`, `close`, `snapshot`, `wait` | Inspect and drive daemon-hosted terminals |
| `project open`, `list`, `close` | Manage the projects every window shares, with or without a window running |
| `channel spawn`, `list`, `close` | Manage the channels every window shares |
| `sessions [--project PATH \| --all]` (alias `ls`), `whoami` | List live agent sessions in this project, or identify the calling session and its peers; see [Hosted agent execution](agent-execution.md#finding-other-sessions) |
| `send SESSION`, `ask`, `reply MESSAGE_ID`, `inbox` | Message another live session, wait for its reply, answer a message, or read the messages left for you; see [Hosted agent execution](agent-execution.md#messages-between-sessions) |

- `daemon status` reads `daemon_version` from the running daemon; it is null
  when no daemon can report its build.
- `daemon stop --require-empty` atomically rejects both established terminals
  and in-flight spawn reservations. `--expected-generation N` additionally
  rejects an inventory that changed since a preflight check.
- `daemon stop --confirm` terminates the counted hosted sessions. Bare, it
  shows the exact count and asks you to type `yes`, so it needs an interactive
  terminal. A script or service passes the capability printed by
  `cagents daemon preflight` instead: `--confirm "$(cagents daemon preflight)"`.
  Either way the confirmation is bound to that daemon and inventory and is
  valid for 60 seconds; inventory changes require a new one.
- A successful stop means **termination requested**, not that every PTY has
  exited. Plain `daemon stop` without these flags is a force stop.
- `terminal spawn` without a command starts the default shell described in
  [Configuration](configuration.md#default-shell); `--cwd` sets its directory.
- Inside a hosted session, `terminal write` and `close` reach only the
  terminals that session controls; see
  [Hosted agent execution](agent-execution.md#finding-other-sessions).
- `project close` asks for `--yes` before deleting the channels a project
  still holds, since closing a project removes them in every window.

## Compatible upgrades

By default, `install.sh` verifies the staged CLI against the running daemon
before switching the `current` symlink. Compatible upgrades keep the daemon PID,
session proof, and PTYs. An incompatible default upgrade stages the new matched
set and keeps the running daemon and its matching UI alive. Finish the update
from the application's update notice or with `cagents daemon upgrade`. The
coordinator retains the running daemon's own CLI, including across successive
staged installs, so a protocol change does not require manually composing shell
commands. Unknown ownership refuses the switch. If publishing launchers fails, the installer
restores the previous version without stopping compatible sessions. Existing
version directories are retained.

Explicit `install.sh --stage-only` also preserves unsaved windows and never
prepares, closes or reopens them. Before publishing the candidate, it records the
validated running owner. While that owner's protocol is incompatible, ordinary
CLI commands and desktop launches use its matching executables. Compatible
candidates can attach to the preserved daemon, so reviewed reopening adopts the
new UI without replacing a compatible daemon. The application handoff remains a
separate reviewed operation, including its save checks.

The application shows available updates above the workspace. Dismissing the
notice moves it into the notices popover; it remains available there. Release
downloads are checksum verified before the candidate's own CLI reports its
protocol. Locally installed builds use the same flow with a verified matched
set of executables.

Before replacing a daemon, the confirmation names each affected session and
whether it continues, resumes, waits for idle, or is blocked. Supported held
Codex servers keep their process, active turn, and approvals. Other native
providers wait for idle by default; choosing interruption explicitly authorizes
ending their active turn before resuming the saved conversation. Accepted agent
launches and automation prechecks must settle first. Changed ownership or impact
requires a fresh confirmation. The final check also waits behind previously
accepted provider commands, so a queued prompt cannot masquerade as idle.

`daemon upgrade` uses this same worker and confirmation. `--yes` skips the
interactive prompt; `--interrupt` additionally permits native turn interruption.
It does not grant approval for a different inventory discovered afterward.
Unsaved window work blocks replacement, and failures remain visible in the
application's update state. A compatible update closes and reopens windows
without replacing the daemon or recovering sessions.

## Full daemon upgrades

A full upgrade replaces the daemon without ending its terminals: each one is
carried across, processes, screen and scrollback intact. Native chats, and
terminals a chat owns, are resumed in new processes. The in-app **Update** action coordinates saving and closing windows
automatically. From the command line, close the application windows and run the
verified installer with `--restart-daemon` (see
[Updating](installation.md#updating)).

Before switching any executable, the installer checks that the old installed
CLI advertises `daemon prepare-upgrade`, calls it with
`--json daemon prepare-upgrade --yes`, and waits for the old daemon to report
that it has stopped. Preparation is daemon-owned: it validates recovery
authority, freezes mutations, durably checkpoints supported sessions, and only
then requests shutdown. Unsupported sessions, in-flight reservations, or stale
authorization reject preparation rather than terminating sessions. The
installer never builds a checkpoint from `terminal list` and never falls back to
an ordinary `daemon stop`.

`cagents daemon prepare-upgrade --yes` can also be run directly; without
`--yes` it refuses before contacting the daemon. Its JSON success command is
`daemon.prepare_upgrade`, with `terminals` and `chats` checkpoint counts.
Success means the checkpoint was persisted and shutdown requested, so callers
must still wait for `daemon status` to report stopped. Prefer the installer's
`--restart-daemon` flow when replacing an installed release.

A terminal is carried by handing it to its holder process, which keeps its
processes running and waits for the next daemon to adopt it. Carried terminals
are never frozen, so their jobs do not block an upgrade.
The next daemon adopts them before relaunching any other terminal. If a
terminal cannot be carried, the old daemon checkpoints it the way it
checkpoints the terminals it restarts, and one whose running job a restart
would lose ends with the old daemon. If a terminal cannot be adopted, the new
daemon restarts it from its safe recipe once nothing of it survives; otherwise
it is logged and not restarted beside its surviving processes. Legacy anonymous
holders expire after 15 minutes. Carrying needs the old daemon to support it, so an upgrade
from a build without it restarts every terminal.

New holders register a private endpoint before the daemon acknowledges terminal
creation. After an unexpected daemon exit, a replacement from the same installation
and boot can authenticate and adopt the original PTY and processes. Live terminals
and completed results remain available until restored and explicitly closed; there
is no timeout that silently kills a running job or discards a completed result.
Each detached holder buffers at most 1 MiB of output, then applies PTY backpressure.
This retains one small holder process per terminal until cleanup. A completed
worker also retains its final screen (at most 8 MiB) in the holder, so another
daemon crash cannot turn a completed command into a new execution. Closing its
card or removing its owning project explicitly releases that retained result.
Retained native/terminal pairs bind the terminal holder's exact process incarnation,
not only a reusable session ID. Private release receipts allow an interrupted
Close to finish after daemon restart. A terminal receipt proves its root and PTY
owner were released (active close also stops the inspected terminal family); it
makes no claim about previously disowned processes outside that family. Native
provider holders separately reap all owned provider/tool descendants before
confirming their release.
The live emulator's screen before an unexpected crash is unavailable; the UI
warns once for the affected pane, preserves subsequent output, and requests that
foreground programs redraw through the PTY. That redraw temporarily toggles one
pixel-size bit and restores the exact rows, columns, and pixel dimensions.

The rest of this section concerns the sessions that are restarted.

An identified agent with no exact resume identity is relaunched into its
provider's resume picker by default (`claude --resume`, `codex resume` or
`omp --resume` with its original options), or as a fresh conversation when its
provider reports it had none yet: the checkpoint records the choice and its
pane says **Relaunched after upgrade**. Turning off **Relaunch agents on upgrade** in
Settings (`relaunch_agents_on_upgrade = false` in `settings.toml`) makes the
upgrade refuse instead and list every blocker. `daemon prepare-upgrade` reads
that setting unless `--relaunch-agents` or `--no-relaunch-agents` chooses for
one invocation; a settings file it cannot read is an error rather than the
default. Unrelated shell jobs always block the upgrade. The installer passes
`--relaunch-agents` only to an installed CLI that can record the acceptance;
without either option it leaves the choice to the installed CLI.

### What is restored

Carried terminals keep their processes, so they lose nothing. For everything
else, the replacement daemon restores supported shell launches and the same
provider-backed agent conversations, including native chats, only after proving
that the previous writers are gone. These are **new processes and PTYs**: shell
memory, foreground jobs, and arbitrary commands are not preserved. Unknown
conversation identity, unsupported command recipes, and surviving or ambiguous
owners block recovery; the durable checkpoint stays available instead of
inventing a fresh conversation or replaying commands. Keep the matching
provider session storage.

Process arguments or environment can be empty during `exec`, so recovery
requires inspectable evidence before restarting sessions. An intentionally empty
environment is accepted only when Linux exposes its initialized zero-length
address range; missing or redacted ranges cannot authorize a duplicate writer.

Daemon startup waiting scales with the checkpoint inventory, including a
provider-confirmation allowance for each native chat. A missing listening socket
does not mean shutdown is complete while the daemon still holds its inventory
lease.

### When the daemon stops without an upgrade

A daemon that crashes or is killed first restores authenticated same-boot
holders as described above. After a reboot, or for older sessions without a
surviving holder, the application recovers each terminal card from its saved
record instead. The daemon keeps that record current for every terminal,
including ones not on screen, by observing the foreground agent periodically.

The daemon also stops itself rather than stay wedged. Everything it hosts
serializes on one internal lock; if a single hold keeps another thread waiting
for two minutes, the daemon logs the stall to `cagentsd.log` and aborts, leaving
a core dump (`coredumpctl info cagentsd`) that names the thread holding it. The
application then starts a replacement, which recovers as described here. Native
conversations survive this only with [durable continuity](#native-provider-continuity);
the others lose their running turn and resume from disk. A
thread that tries to take that lock twice panics on its own instead, which is
recorded in `cagentsd-crash.log` and ends that thread's work rather than the
daemon's.

| Saved record | Recovery |
|--------------|----------|
| An exact agent conversation | Resumes that conversation |
| An agent recognised without an exact conversation (for example mid-`/clear`) | Opens the provider's resume picker in the same directory with the original options |
| A plain login shell | Starts a new shell |
| Any other command | Waits for **Retry**; it is never replayed automatically |

Each decision is logged at `info` level with the terminal ID, program and
working directory: the checkpoint in `cagentsd.log` and card recovery in
`cagents-ui.log`, both in the cagents data directory
(`~/.local/share/cagents` by default).

### Failure and rollback

If publication fails after checkpointing, the installer restores the previous
version and starts its daemon against the durable handoff. A replacement startup
failure rolls back only when its status proves it stopped; if startup state is
ambiguous, the installer keeps the new installation and recovery state rather
than risk a second writer. Inspect `cagents daemon status` before retrying.

A retained native conversation that its provider refuses to reattach, for
example because Codex no longer finds the thread's rollout in its account
storage, does not fail the restore. The replacement daemon still starts and
adopts every carried terminal and other session; nothing is started or resent
for that conversation, including a paired terminal view the checkpoint would
otherwise relaunch, and its provider holder keeps running. The exact checkpoint
entries and the provider's reason are recorded in
`daemon-upgrade/daemon-upgrade.unattached.json`, `cagents daemon start` and
`cagents daemon status` warn about each such conversation (JSON output carries
them under `upgrade_attention`), and while its holder remains its card shows
the reason when reopened. Each later daemon start tries again to reattach the
retained holder; the record is cleared once the conversation is reattached or
explicitly closed, or at the next daemon start after its holder is gone (for
example after a reboot), when only the log names it for manual recovery from
provider storage. Failures that concern the checkpoint itself still stop
startup and keep it replayable.

An interrupted checkpoint is never blindly replayed. After confirming that every
previous provider writer has stopped, `cagents daemon abandon-upgrade --yes`
archives the retained checkpoint without resuming any process. It requires a
stopped daemon and refuses to overwrite an earlier archive. Reopen the exact
conversations manually from their provider session storage; do not delete
checkpoint evidence just to get past an ownership error.

### Legacy installations

Installations whose CLI lacks `daemon prepare-upgrade` cannot perform this
handoff, and `--restart-daemon` refuses without changing the active
installation or stopping sessions. For the one-time bootstrap, finish those
sessions or explicitly terminate only disposable ones with the installed CLI,
wait for its daemon to exit, then install the new release normally. This
bootstrap does **not** preserve legacy PTYs or manufacture conversation
recovery.

Closed windows keep their recovery lineage across successive full upgrades;
they do not need to be opened for every release.

## Native provider continuity

A newly owned native Codex server runs under a small dedicated holder. Once
durable continuity is confirmed for it, as described below, the holder remains
alive when its daemon connection disappears; a holder without that confirmation
ends its server with the connection. The holder owns only the provider
process tree and bounded private control metadata; it does not run the UI,
terminal emulator, transcript store, or a second protocol actor. It never sends
a prompt or answers an approval. Explicit close terminates and reaps the entire
owned process tree, including tools that changed their process group, before
acknowledging cleanup. A private cleanup receipt records only the exact process
and daemon identities after every descendant is reaped. If the Release reply is
lost, a later close verifies that receipt; a missing socket or process alone is
never treated as proof. Explicit chat close retains the minimal authenticated
receipt for exact disk resume; other transport teardown consumes it. A paired
terminal must also acknowledge teardown before that receipt authorizes resume.
Across a kernel boot change, the old process tree is necessarily gone, so an
exact saved conversation can resume without the ephemeral runtime receipt.

Durable continuity is enabled only after the actual server confirms its exact
thread, account directory, process identity, and a tested version. The
allowlist is `VERIFIED_CODEX_VERSIONS` in `chat/native_holder.rs`, currently
Codex **0.157.1** and **0.160.1**. It names exact versions, so every Codex
update leaves it behind until that version is qualified.

A Codex conversation on any other version does not acquire this claim, and its
server does not outlive the daemon. When the daemon crashes, is killed, or
aborts itself, the holder ends the server's process tree: the turn in progress,
its running commands and its pending approvals are lost, and the conversation
returns by resuming from disk. The daemon logs a warning to `cagentsd.log`
naming the thread and the reported version when a conversation starts without
durable continuity for this reason, once per conversation. Other providers
retain their existing reconnect/resume behavior.

To qualify a version, add it to the allowlist locally and run the
qualification against the native Codex executable:

```sh
CAGENTS_QUALIFY_CODEX=/path/to/native/codex cargo test --locked \
    --no-default-features --features host \
    --test codex_continuity_qualification -- --ignored --test-threads=1
```

It hosts that executable in an isolated daemon, with a local stand-in for the
Responses API, and kills the daemon with `SIGKILL` during a long-running
command, during a poll of a yielded command, and while an approval is pending.
Each case passes only if the replacement daemon reattaches the same server
process and thread, the turn finishes there, the model endpoint received each
prompt once and no request it did not need, and the approval stayed unanswered
until a client answered it. Keep the entry only if all three pass. A killed
daemon is how a server meets connection loss in practice, so there is no
separate connection-loss case.

A replacement daemon must hold the installation's exclusive lease and match
the private holder's boot, process identities, and exact issued descriptor
lineage. Removed account storage does not invalidate the proved server's process
identity, but successful attachment still requires that server to confirm the
thread. Codex 0.157.1 can return metadata for an unloaded thread or a live turn
waiting on approval while refusing to attach because its rollout is missing,
even when no history page is requested. That failure preserves the holder
and reports unavailable history; metadata alone is not live continuity. This
exception cannot authorize a new process or recreate the removed directory.
A missing, exited, or mismatched holder refuses automatic
recipe execution. Failed local actor installation preserves the retained server
for another authenticated attempt. Paired terminal views must independently
recover their own holder before the native pair is published.

When unavailable history prevents attachment, Close still authenticates the
exact retained owner and tears it down without loading history. The card keeps
its descriptor until cleanup is confirmed. Private teardown receipts retain
hashes of issued descriptors, allowing a lost close response to be retried after
a daemon restart without interpreting a missing process as successful cleanup.

Execution consoles retain their exact daemon-issued recipe in the native
holder, independently of the provider listener. The holder acknowledges pair
changes before they become visible. Adoption advances the retained execution
capability and agent descriptor together; uncertain accepted runs remain
interrupted and are never resent. A scheduler's held working-directory file
descriptor reaches the provider launch through the holder. After recovery,
read-only attachment and cancellation remain available if that path changed,
while new work and approvals still require the saved directory identity.

## Recovering after a daemon change

When the daemon incarnation changes, cagents authenticates the replacement and
checks for surviving owners before recovering safe shell or exact agent
launches. Unknown or orphaned processes block recovery rather than risk
duplicate writers. **Retry** explicitly authorizes saved launch recipes that
cannot be replayed automatically. Transcripts remain available; recovery starts
or adopts a proven replacement, not the old terminal process. Activity evidence
never authorizes conversation recovery.

## Debian packages

Debian upgrades never send signals to daemons. The package records its protocol
and keeps root-private executable-digest evidence under
`/var/lib/cagents-package-protocols/`, allowing an older compatible executable
to keep running across repeated upgrades. A running incompatible or unknown
executable makes `preinst` fail visibly; the administrator must arrange an
explicit user-owned shutdown before retrying. Removal similarly refuses while
that package owns a running daemon, even an empty one. Package scripts do not
connect to user IPC as root or inspect terminal contents.

An already-installed legacy package's outgoing maintainer scripts cannot be
changed by the incoming package; stop its daemon explicitly before the first
transition to this policy.

The upgrade policies are covered by repeatable checks described in
[Development](development.md#installer-and-package-checks).

## Development instance

Cargo debug builds, and builds with the explicit `development` feature, use a
separate daemon, session store, application lease, and runtime namespace
(`cagents-development` in place of `cagents`). The development daemon shows a
badged tray item with its own name, so it can run beside the installed
production application. It does not register an autostart entry. Release builds
without that feature are production builds.

Manual sidebar order is stored by the selected device daemon and shared across windows. Drag worktree or channel titles within a project to arrange them; the project menu’s **Reset sidebar order** restores automatic channel and worktree sorting. Missing worktree paths retain their position if they reappear. The registry bounds each retained order at 4096 entries; resetting also discards obsolete paths. Navigation changes require a matched CLI, UI and daemon (wire protocol 56).
