# Native chat capacity

The daemon keeps a bounded number of resident native chat handles. Completed
handles can still occupy this capacity. This limit is separate from account
usage allowances and provider plan quotas.

Run `cagents chat capacity` to see used admission slots, resident handles, pending
spawns and the configured limit. Its compact resident table includes completed
handles that ordinary session listings may omit. The table shows at most 4096
rows and reports any omitted count; it does not load transcripts or establish
release eligibility. A pending recovery can refer to an existing resident, so the
used count can be lower than resident plus spawning. `cagents chat capacity set 256` persists a new limit on the
selected daemon. The default is 128; accepted values are 1 through 4096. The
upper bound limits an accidental configuration change and does not guarantee
that the machine can run that many providers. Increasing the limit can increase
memory and process use.

Lowering the limit leaves existing residents and their work untouched. New
admissions wait until usage falls below the limit; exact recovery of an existing
resident does not allocate another slot. The daemon saves this setting in
`native-chat-capacity.toml` beside its window settings, with a private atomic
replacement. Window settings saves cannot overwrite it. A malformed or unsafe
capacity file causes daemon startup to report an error rather than silently
change the limit. If the atomic replacement succeeds but directory syncing fails,
the new limit takes effect and the CLI reports a durability warning.

Cooperative upgrades and retained recovery inventories use the same 4096-chat
ceiling for stored inventory. Recovery still obeys the destination admission
preference when it creates resident handles; set that preference high enough
for the inventory being restored. The checkpoint and each recovery evidence
file still have an 8 MiB limit; source inspection retains its
4 MiB response limit and 30-second deadline. Large launch environments or long
recovery lineages can exceed these byte budgets before reaching the count limit.
An oversized checkpoint refuses the upgrade before stopping source owners; an
oversized recovery report preserves existing evidence and the replay checkpoint.
Reduce the inventory or launch environment and retry. This count bound does not
qualify 4096-provider performance or a multi-pane native upgrade.

## Repair a capacity file that prevents startup

`chat capacity set` needs a running daemon. If this setting prevents startup,
repair it offline on the selected daemon's host. Do this only for the daemon
that failed to start; leave a healthy daemon's configuration alone.

First select the correct application instance. On Linux, the configuration
base is an absolute `XDG_CONFIG_HOME`, or `~/.config` when that variable is
unset or relative. The file is:

- Production: `cagents/native-chat-capacity.toml` beneath that base.
- Development: `cagents-development/native-chat-capacity.toml` beneath that base.

Debug builds and builds with the `development` feature use the development
instance. A remote device uses its own host's configuration base, not the
local CLI's directory.

For a malformed regular file owned by your user, preserve the existing file by
renaming it to an unused backup name in the same directory. Check that the
backup name is absent and use a move that refuses to overwrite an existing
file; do not replace an earlier backup. Then choose one recovery:

- Leave the capacity file absent and start normally to intentionally restore
  the default limit of 128.
- Create a private replacement regular file owned by your user, with mode
  `0600`, containing only `limit = 128` or another chosen integer from 1 through
  4096, then start normally.

If startup reports an unsafe path, inspect the path components and ownership
before repairing them. Do not follow a symlink to edit its target or replace
files whose ownership you have not established. This procedure changes only
the capacity preference; leave saved conversations, drafts and provider files
untouched.

To inspect one handle, run:

```
cagents chat release FULL-CHAT-ID --dry-run
```

An idle activity label alone does not authorize release. The daemon checks the
exact resident, provider state, ownership, transitions, attached readers and
whether saved history can be reopened. The inspection reports why release is
blocked. Running, waiting or unconfirmed work remains protected. Release supports
detached, unmanaged Claude history readers and writers whose provider teardown
has finished successfully. Every cached item, metadata field and body chunk must
be covered by a fresh local history reload before its resident cache can be
released. Its own delegation capability is revoked only after confirmed teardown
and an exact process-identity check. Managed bindings, pending launches and
retained delegated agents block release; use their supported owned-agent close
operations first. Reopening uses read-only history and never starts the provider.

A completed writer may still be blocked: live observation timestamps, unsaved
messages, child transcripts or private image files can make its cache differ
from saved provider history. Those residents remain protected. A finished run
alone does not establish that dropping its cache preserves the full transcript.
The command is not a general cleanup operation for every Idle or completed row.

If inspection confirms eligibility, `cagents chat release FULL-CHAT-ID --yes`
explicitly requests release. The CLI obtains a fresh identity proof and the
daemon checks it again before changing anything. A changed daemon, successor
actor or snapshot generation invalidates the proof. Release drops a resident
handle; it does not delete saved conversations. There is no bulk cleanup or
automatic eviction. After capacity becomes available, retry the blocked pane's
normal start or reconnect action.
