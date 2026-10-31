# PR1310 matched dev-runtime build producer — source only

This four-file task slice can select actual Cargo artifacts and copy each arm's
matched CLI/HOST/UI ELFs into exclusive private evidence before another arm
changes the shared target. It contains **no process supervisor, native launcher,
C-lock owner or authority mechanism**. The public `produce_arm` path first refuses
because `ORIGINAL_COMMAND_BINDING` is NULL. No actual discovery/build/artifact or
chain/config/import result exists; this is not a runnable or grant-ready packet.

The source is based on reviewed72fd. Existing entry/library/driver/fixtures and
all their behavior remain byte exact. Native source tuples are Before4ad36779
and After02802c8a. Build commands reproduce the browserless branch of each actual
`scripts/run-development`, lines191–196, adding only JSON output presentation:

- `cargo build --locked --profile dev-runtime --no-default-features --features development --bin cagents --message-format=json`
- `cargo build --locked --profile dev-runtime --no-default-features --features development,host --bin cagentsd --message-format=json`
- `cargo build --locked --profile dev-runtime --no-default-features --features development,ui --bin cagents-ui --message-format=json`

The compiler path is the physical Rust1.93 toolchain, jobs2 and the fixed existing
lane-C target/lock. No union, alternate target, new cache or cold fallback exists.
Per arm, a future owner clamps Zig30 + CLI600 + HOST600 + UI900 and all metadata /
copy work to its **one original2400s budget** (2130 planned,270 overhead). Missing
warm artifacts or exhausted time refuses; this source never extends the budget.
The development runner is still required for later native launch. These build
commands do not launch the app or prove the installed shell/Cargo-run chain.

## Concrete callable slice and the missing safe owner

`produce_arm(arm, checkout, evidence_root, inputs, retained, invoke, fence, failure)`
is implemented but refuses before effects until a future, independently reviewed
source composition binds the exact retained original owner callbacks. The three
callbacks must be bound methods of the same actual original object, with exact
source file/hash/qualname bindings. Those checks only correlate source; they do
not establish User authority, an authenticated resource grant, C ownership or a
safe callback from caller-positive JSON. No caller input can fill the current
NULL source constant. The original owner must retain `retained` at
`owner.value['matched_build_producer']` before this call, so every partial
artifact, acquired FD, close result and error remains in original custody.

The candidate inspected is the maintained `run-host-lane.py` Runner.captured
and d9ee Custody.capture: original retained process objects, waits/pidfds and
COMBINED output, followed by family observation. Its current HOST/canonical-slot
plan, prior-return admission, no per-command environment parameter and uncapped
capture are **not** a supported native matched-build binding. No import, adoption
or retarget occurred. A safe future original owner must bind the real source
checks/contexts/cancellation/remaining time, allow the exact child environment,
retain actual Popen/wait/output/child-status facts before fallible pins, and refuse
unknown descendants without killing them. The old A2 `settle_children` can kill
newly adopted children; none of it, d610, fa6 EMPTY0 or any cleanup is copied here.
A2's source review is dependency context, never authority for unsafe behavior.

The existing source-owner names used are `check_bindings`, `source_observation`,
`paths.checkout`, `plan['source']`, `value`, and `save`; their actual safe task
binding is also absent. `invoke` must return the actual original result and
retained command record, with combined raw bytes, original exit, distinct cleanup
exit, output hash and unforced original-family observations. A dict of positive
flags without that reviewed original source is unsupported. `fence` must enforce
the same actual original deadline/cancellation and source/context ownership, with
no timeout reset. `failure` must latch partial/error state on that original owner.
This slice never signals, waits/reaps/closes a process or closes/acquires C.

## What the producer actually records when eventually admitted

Before builds, it observes the exact source gate and physical executable bytes,
runs the selected real Zig's `env` through that future original callback, and
requires reported `zig_exe` to match the selected physical `zig` and version0.15.2.
It then prepends the verified Zig directory before physical Rust1.93 and the
unchanged caller PATH, as the runner does. Unexpected effective Cargo selection
or a conflicting inherited CAGENTS_ZIG is refused, never repaired. Original HOME,
CODEX_HOME, opaque authority/account variables and the worker environment are
unchanged; only explicit compiler/target/jobs and Zig-first child-PATH overlays
are requested from the original command owner.

All original combined bytes remain the original owner's evidence. The selector
parses actual JSON lines among retained progress/warnings: one successful
build-finished, one exact root cagents0.1.3 package/manifest/source, bin kind,
features, non-test actual profile and Cargo-reported absolute executable under
the fixed target/dev-runtime path. Malformed JSON, duplicate artifacts/completion,
compiler errors, wrong sources/features or unknown original status refuse.
The name `dev-runtime` comes from argv/source, **not a nonexistent JSON profile
name**. Actual profile fields and the raw Cargo `fresh` boolean are retained. Cargo
reports `fresh=true` for an up-to-date artifact without running rustc, and
`fresh=false` when rustc ran. This is Cargo-reported status, not independent
timing or full-chain proof. The input stream is never relabelled separate stdout.

Each copied ELF is source identity/hash bound across the actual bounded copy,
with an actual x86_64 ELF header and private exclusive0700 destination, then
fsync and one-shot close facts. Normal Cargo hard links are allowed on the
source; its exact identity/link count must stay stable. Private copies and0600
per-bin JSON receipts are created exclusively with O_NOFOLLOW|O_EXCL, never
hardlinked, overwritten, deleted or repaired. Partial failures stay retained.
Before the next graph/arm, actual immutable copy and receipt must have returned.
This does not make a filesystem immutable against the owning UID or establish
complete process/native dependency closure. The returned `matched` paths name
immutable evidence copies, not ready native-owner runtime identities. Actual
original executable paths are in each `actual_Cargo.executable`. A future real
binder must separately correlate the normal `scripts/run-development` original
UI path/incarnation/hash with retained copy evidence before native effects and
satisfy the existing native-owner `exact_matches` contract. Never execute a
copied app directly or substitute a copy path into existing native sources; no
runtime mapping or adoption is implemented here. Stdlib transient metadata opens and
kernel calls remain cooperative; no global FD/resource certificate is claimed.

Ancestor and CARGO_HOME config files and selected environment inputs are observed
as bounded identities/digests, without private values. Includes, wrappers, shims,
target-specific runner semantics and the full effective Cargo configuration are
still UNKNOWN. Physical binary hashes and a version banner cannot fill that gap.
No full import/native/renderer/UX or later application inference is produced.

The six `MatchedBuildSourceOracles` parents are AUTHORED UNRUN. They select only
actual pure source AST; no full producer import or effect/orchestration body is
executed during preparation. Source/JSON/AST/hash/inverse checks are preparation
only. Existing native scale five-oracle UNRUN and historical pure3 scope are
unchanged. Independent source review and an actual one-use original C grant are
required before any discovery/build; all future runtime fields remain NULL.

## Root library/bin sibling correction (source only)

The immutable fb5 source negative is retained: the ordinary root library and CLI
binary both have target.name `cagents`, so name-first validation wrongly rejected
the lib row before reaching the actual bin. The selector now filters actual bin
kind before applying every existing exact requested-bin guard. It still refuses
lib-only streams, duplicate bins, wrong selected sources/features/profile/paths,
errors and ambiguous completion. One new CLI parent uses actual `commands()[0]`
with both rows in the SAME original combined stream; its lib-only and duplicate
cases are nonadditive. All original five parent bodies/UI assertions are exact.
All six remain AUTHORED UNRUN; no executable import/function/case was run. NULL
safe-owner binding, first-effect refusal and all chain/import/resource holds are
unchanged. A fresh focused nonauthor source review is required.
