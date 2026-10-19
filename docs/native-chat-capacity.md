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

To inspect one handle, run:

```
cagents chat release FULL-CHAT-ID --dry-run
```

An idle activity label alone does not authorize release. The daemon checks the
exact resident, provider state, ownership, transitions, attached readers and
whether saved history can be reopened. The inspection reports why release is
blocked. Running, waiting or unconfirmed work remains protected.

If inspection confirms eligibility, `cagents chat release FULL-CHAT-ID --yes`
explicitly requests release. The CLI obtains a fresh identity proof and the
daemon checks it again before changing anything. A changed daemon, successor
actor or snapshot generation invalidates the proof. Release drops a resident
handle; it does not delete saved conversations. There is no bulk cleanup or
automatic eviction. After capacity becomes available, retry the blocked pane's
normal start or reconnect action.
