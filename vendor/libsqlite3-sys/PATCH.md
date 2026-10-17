# cagents bundled SQLite backport

This fork retains the `libsqlite3-sys` 0.35.0 Rust API and `sqlite3` links
identity required by rusqlite 0.37, Servo storage and the browser importer.
The native SQLite implementation is the complete upstream **3.50.7**
amalgamation, including the upstream WAL-reset fix. It is not a renamed
3.50.2 source with an isolated hand-written hunk.

The original crate is the crates.io 0.35.0 archive, SHA-256
`133c182a6a2c87864fe97778797e46c7e999672690dc9fa3ee8e241aa4a9c13f`.
The native source is SQLite Fossil check-in
[`c7facf7ac58d2cdac619a2da838c5fd5c5270f52b3458c7881b5a4fca1ca4791`](https://sqlite.org/src/info/c7facf7ac58d2cda),
whose parent is the official checkpointer/writer wrap-race backport
[`268c9da28706e163e2dfe65a9848e007ef4adc3cc4764042f0081830b88c4a1d`](https://sqlite.org/src/info/268c9da287).
The native version/source ID must match both the runtime library and the
version constants in the two bundled Rust binding files. Other Rust binding
and native build logic remains the original 0.35.0 code.

SQLite explains the affected concurrency pattern and fixed versions in its
[WAL-reset bug documentation](https://sqlite.org/wal.html#walresetbug).
cagents retains independent model and execution WAL connections. A passing
concurrent-write/checkpoint regression checks their functionality; it is not
proof that the rare upstream race was reproduced in the old version.

## Reproduce the native amalgamation

Use a Linux development environment with a C compiler, GNU make, Tcl, tar and
sha256sum. Regeneration is offline after obtaining the pinned archive. It
builds generator tools, so serialize it with other project compiler work.

```sh
work=$(mktemp -d)
curl --fail --location \
  'https://sqlite.org/src/tarball/sqlite-3.50.7.tar.gz?r=c7facf7ac58d2cdac619a2da838c5fd5c5270f52b3458c7881b5a4fca1ca4791' \
  --output "$work/source.tar.gz"
scripts/generate-bundled-sqlite "$work/source.tar.gz" "$work/generated"
```

The script checks archive SHA-256
`5aaadc14483389983751300f3f1072bb630e846c71cd6aaaa1bb3aed4f8fb65c`,
checks the exact manifest/version, runs upstream configure and `make sqlite3.c`
with a clean environment, and requires the exact unmodified source ID. It
writes all three complete native files and their hashes to the fresh output.
The generator output is portable source; target-specific compilation and
feature flags remain in the unchanged crate build script.

Compare generated `sqlite3.c`, `sqlite3.h` and `sqlite3ext.h` with `sqlite3/`.
`UPSTREAM.json` records the checked-in file hashes and generation provenance.
The crate's original `upgrade.sh` predates this backport and targets 3.50.2;
use the project recipe above for this fork.

The supported application configurations select ordinary bundled SQLite.
The upstream SQLCipher sources and system-library escape hatches are retained
for crate provenance and are not selected or qualified by this change. This
does not change journal settings, the database file format, application schema
versions, production data, or sessions.
