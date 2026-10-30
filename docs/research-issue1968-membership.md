# NEVERMERGE: Issue 1968 candidate qualification

This temporary research branch qualifies the literal membership candidate
`c96008f4f7e7e6d8d96a5d34d0dcc388bfc970ab`, tree
`e28a2f909181a92332e8ee1183ef21f4057d156b`. It is not a product PR and must
never merge. The lead owns review, publication and any hosted execution.

The workflow uses one Ubuntu 22.04 runner and sequential Cargo invocations with
Rust 1.93.0 and `CARGO_BUILD_JOBS=2`. Checkout, upload and Linux setup pins match
the prior reviewed PR 1931 research controller. Permissions are `contents: read`;
checkout does not persist credentials. There are no provider, session, PR-token
write, dispatch or authority-substitution operations.

Before and during qualification, any inherited `CAGENTS_*` name rejects the run,
including empty values, all `CAGENTS_TEST_*` names and both private pathspec
overrides. Evidence contains names and presence booleans only. The controller
does not remove or rewrite authority variables. Private TMPDIR/XDG fixture paths
and a Cargo target outside the source checkouts are the only fixture setup.

The candidate source, Cargo manifest, lockfile and both frozen scale harness
files have literal SHA-256 pins in the controller. Product head/tree, research
event head, setup head/tree/action hash and clean checkout states must match
before and after execution. Original source files and the complete control are
copied into the artifact before commands run.

The bounded lane runs:

1. Formatting, isolated HOST graph and `cargo check --locked
   --no-default-features --features host --bin cagentsd`.
2. HOST `--lib` selectors `large_membership` (four),
   `large_unstage_with_a_ghost` (one), and `small_membership` (one, including the
   original >17 MiB sparse-index/tree regression). Each selection is listed and
   checked against its exact source-derived expected names before execution.
3. Six exact existing semantic parents: selected/unselected index preservation,
   unborn unstage, index-lock refusal, rename-source handling, empty unborn
   membership and literal mutation command shape. Each expects one test.
4. HOST integration target `repository_scale_contract`, exact selector
   `large_literal_pathspec_mutations_roundtrip_without_argv_limits`: one outer
   owning test and its one private child. Missing or zero-test selections fail.
5. Separate isolated CLI and browserless UI graph/check invocations. UI must not
   resolve Ghostty; HOST must not resolve libcosmic, Servo, WebRender or
   SpiderMonkey. The HOST and UI features are never combined.

The scale source remains byte-for-byte unchanged. Its 240-second private-child
guard includes contract execution and family settlement; output remains capped
at 1 MiB per stream. The workload still generates 40,000 files and asserts a
stdin pathspec larger than 2 MiB. Every original mutation, literal-name and
tracked-content assertion remains active. The fixture alone creates its private
child-mode variables, becomes its family subreaper, waits for ECHILD, and removes
its namespace only after the owner verifies success. Failure or timeout retains
the namespace. The controller adds no deletion trap, outside timeout or signals.
No separate `cagentsd` helper is needed by these repository tests.

Every controller-managed command retains its exact argv, cwd, original exit, raw combined
stdout/stderr, wall time, child CPU counters and SHA-256. Peak child RSS is
explicitly cumulative within each controller stage, not a per-command claim.
The original scale phase diagnostics are retained and must all finish on a
passing run. Hardware and build tools are recorded. Always-run steps retain
final source/clean-state observations, skipped/unrun command status and an
artifact file hash inventory even after an ordinary command failure. Hidden
source paths are explicitly included in the artifact. The pinned setup action's
installation output remains in the original Actions job log. Synchronization
does not cancel a running qualification. Explicit cancellation, the 180-minute
job limit or runner loss can prevent final capture; such a run is incomplete.
Failure remains failure;
there is no allow-failure behavior or broad passing claim.

This is candidate-only qualification. It does not measure a same-hardware
before/after speedup, establish full HOST all-targets coverage, exercise a native
UI flow or qualify the integrated-browser graph. Those claims remain unrun.
Preparation of this branch itself performs no builds, tests or hosted actions.
