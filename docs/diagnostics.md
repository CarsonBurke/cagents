# Private production diagnostics

`cagents diagnostics` reads local, retained evidence without starting the daemon,
opening a session database, preparing a remote device, or routing to a staged
installation. `cagents diagnostics --json` returns the ordinary CLI envelope;
its `data.schema_version` is `1`. `--device` is rejected: run this command on the
device whose evidence you need.

The UI, daemon, and standalone ceditor write typed records to
`$XDG_STATE_HOME/cagents/diagnostics`, or `$HOME/.local/state/cagents/diagnostics`
when an absolute XDG state directory is not configured. The development identity
uses `cagents-development` instead of `cagents`. The application and diagnostics
directories must be owned by the effective user and have mode `0700`; files must
be owned regular files with mode `0600` and one hard link. Ancestor symlinks and
unsafe writable ancestors are refused. Directories are walked through retained
file descriptors, and records are appended through freshly validated descriptors.
Successful append acknowledges both file data and directory entries; newly
created private directory names are synchronized through their parent.

Ceditor records its own compiled package version, not the shared cagents library
version. Its `--help`, `--version`, and `--settings-path` commands create no
production recorder.

Each product retains five JSONL files, each at most 2 MiB, plus an empty stable
lock file. Cooperating windows and crash/restart loops share that product budget.
An oversized existing file, unsafe replacement, or failed rotation stops the sink;
it never appends beyond the cap. Failure is non-fatal and emits one fixed notice
to standard error and, when a UI is available, its notice history. Lock contention
waits at most 50 ms, skips the current event, and leaves the recorder available
for later attempts. The next successful append records a typed skipped-event
count; one fixed notice reports the gap. A run with a gap cannot claim a clean
shutdown even when a later exit succeeds. Existing raw
logs from older versions are preserved; they are not part of this new sink.

Records contain a schema version, product, process/run identity, timestamp,
compiled version, protocol number, and a closed event/category vocabulary.
They never accept terminal bytes, prompt text, arguments, environment values,
paths, URLs, raw error messages, panic payloads, or backtraces. Persistence evidence
includes classified failures and recovery, writer loss, and a final-flush record
with unsaved/refused counts and the clean-marker outcome. Settings failures whose
original API supplies only text are classified as `unknown` rather than parsing
that text. A successful process exit only earns `clean_shutdown: true` when its retained
persistence evidence is complete. A missing final-flush record leaves that
claim unknown; failed final persistence or settings makes it false. Inspect
`persistence`/`final_flush` for the classified outcome.

JSON reports contain up to 256 redacted recent records per product and a
`last_observed` summary for the most recently recorded run. Versions, protocol,
and health are **last observed**, not live sibling executable or process checks.
The human report also shows the latest retained typed failure with its time and
run, labelled historical because it may have recovered; it does not change the
last-observed run's health. The failure line is limited to the recent record
window. If a run's startup has fallen out of that bounded window, its clean
shutdown remains unknown even when its final persistence result is retained.
Missing, unsafe, malformed, or truncated evidence has unknown health. A fresh run
never inherits a previous run's clean shutdown or successful flush. When no final
flush was recorded, the command cannot claim whether unsaved writes survived. Abrupt termination, including a daemon
watchdog abort, may leave only startup/readiness evidence; its exact cause is
unknown rather than guessed from a missing exit record.

Raw debug output remains local and explicit. Use
`scripts/run-development --browserless --debug --log-file /var/tmp/cagents-ui.log`
or explicitly set `CAGENTS_LOCAL_DEBUG=1` when invoking a binary. This enables
foreground standard-error tracing and the ordinary raw panic hook; `RUST_LOG`
only chooses its filter and does not enable raw capture on its own. Raw debug
output can contain application data: its caller-selected file has caller-managed
retention and is never returned by `cagents diagnostics`. No support upload or
network telemetry is performed.

`cagents diagnostics` also reports the current transcript-cache directory count
and total regular-file bytes under this instance's runtime directory. This is a
filesystem observation, separate from recorded product health, and never starts
or contacts the daemon. A missing runtime directory reports zero; an unreadable
one reports unavailable. It counts active and orphaned caches together.

Chat caches hold an exclusive per-directory lease while live. Daemon startup
reclaims unlocked caches; legacy caches without a lease are retained while a
same-user process has an open descriptor beneath them, or procfs cannot establish
that safely. Creation and sweeping share a gate, so the interval between mkdir
and lease acquisition cannot expose a live new cache to deletion.

The cache remains in the private runtime directory: history and decoded images
are ephemeral and are removed at Store drop or following daemon restart. Its
SQLite connection is actor-private, so it uses DELETE journaling with a zero
retained-journal limit and synchronous writes disabled. Journals exist only for
an active transaction and disappear at commit; a large transaction can still
need a transient rollback journal proportional to pages changed. Transaction
failure retains rollback semantics; process-crash recovery is unnecessary because
the next daemon removes the entire orphaned cache. The page-cache budget is
1 MiB per connection. Moving long transcripts to disk-backed cache storage remains
a possible separate change: active transcript size still consumes runtime tmpfs,
but stale generations and historical WAL rewrite volume no longer accumulate.
