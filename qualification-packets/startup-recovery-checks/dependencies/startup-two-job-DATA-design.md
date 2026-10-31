# eb7 startup qualification: two bounded serial jobs, DATA only

Additive refinement of `/tmp/b914f9d9-startup-composition-minimal-validation-design.md` (SHA256 `f20a4319cb3ca7916ae90d82a1352a58e7b32a26c298b09ddea139750156bfe1`). No execution packet/code authored or imported; no build/test/native/helper/context/resource/process/target/DB probe, network write, environment change or source mutation. All execution remains UNRUN. Root16 owns #2025 and its active controller.

Exact qualification source: `eb7ff16beb8864349573231521c5d4eab5270cc6`, tree `8d60fb0154109a82861f9b595f2b7296e4f55c26`, parent `b914f9d9b55527aa1bb22d591968c58958d717ee`. Immutable Git reads establish one114-line insertion in control.rs only. Independently removing that insertion gives byte-exact old control.rs; all other14 files in the b914 feature freeze are byte-identical. New control.rs SHA256 `5ffc695ec8029d10808e05647d5bd106ff51e2e0b728b9a3b6ed3f2404e410c1`. Source review `/tmp/b914-shared-startup-transport-test-independent-review-v1.json` SHA256 `fcfa4aa9cb29c643668a0887c9d77e4d2d60d4a024783a4466c5282bf1a47015` verified as a file pin; no runtime result inferred.

## Conditional executable-source prerequisite

Preferred donor is Root16 actual SourceB grouped controller/parser ONLY after its original #2025 execution, independent RAW acceptance, positive original C return, and exact frozen code/review/manifest pins are available. Those facts/pins are not supplied or inspected here and remain future admission. A reviewed proposal is not that donor. Keep #2025 source/results immutable; prepare new task-specific derivatives only after intake. No approval, runtime grant or result transfer is implied. EOF/CLI5 single-parent route remains fallback, not this preferred queue.

Reuse the returned SourceB original custody/wait/pidfd/subreaper, sticky failure, cancellation, source/context, tool binding, warm-target lock and safe-close/HOLD code. Reuse its actual grouped selection/result parser rather than inventing a UI runner. Review task-specific edits against that exact code and its actual conservation census; do not assume the old proposal is byte-identical to actualB. No new helper/target/cache/lane/framework or parallel cold Cargo.

## Queue and finite budgets

After #2025 actual return and fresh Root admission: job A runs and positively returns; job B then receives its own admission and runs. They are two separately declared source-bound jobs, each with its own original2400s deadline from its own acquisition. They are not one workload whose timeout is reset. Every phase uses min(remaining job budget, own cap); no live extension or automatic retry. Any genuine nonzero/timeout/mismatch is FAIL_STOP with its original negative retained. Unknown custody keeps the existing indefinite HOLD;2400 bounds workload, not safe-release latency.

Both use the existing lane-C target/lock, physical toolchain/job policy from actual returned SourceB and future reviewed admission (historical proposal: Rust1.93/jobs2). No current warmth/context/tool availability was probed. Source/head/tree/clean status, all source/config/controller/data/review pins and context are checked before/through/after each job. Job B must reference actual A completion/return evidence and require matching product/tool/build-config fingerprints; no results combine across an unexplained changed source/context. Natural HOME and opaque authority stay intact.

Job A: three isolated binary checks, serial,600s per phase, sum1800s within its2400s workload cap. No test ELF or test-group results requested; zero parent/group counts are legitimate for this declared check-only job.

```text
cargo check --locked --no-default-features --bin cagents
cargo check --locked --no-default-features --features host --bin cagentsd
cargo check --locked --no-default-features --features ui --bin cagents-ui
```

Job B: nine serial phases, traversing each artifact group in existing compile→inventory→group order: shared CLI then HOST then UI. Zero binary checks here. Proposed task DATA caps: each compile600s; each inventory30s; CLI group60s; HOST group60s; UI group300s. Sum2310s leaves90s under its2400s cap for controller/source checks. These tighter compile caps are a task data choice, not a timing/warmth claim; exhaustion stops unqualified. No scheduler-degree/parallel-overlap assumption is used to justify them.

```text
compile-cli-lib:  cargo test --locked --no-default-features --lib --no-run --message-format=json
inventory-cli-lib: ORIGINAL_CARGO_EMITTED_CLI_ELF --list --color=never
group-cli-lib: ORIGINAL_CARGO_EMITTED_CLI_ELF CLI_FULL_NAME --exact --format=pretty --color=never
compile-host-lib: cargo test --locked --no-default-features --features host --lib --no-run --message-format=json
inventory-host-lib: ORIGINAL_CARGO_EMITTED_HOST_ELF --list --color=never
group-host-lib: ORIGINAL_CARGO_EMITTED_HOST_ELF HOST_FULL_NAME --exact --format=pretty --color=never
compile-ui-lib:   cargo test --locked --no-default-features --features ui --lib --no-run --message-format=json
inventory-ui-lib: ORIGINAL_CARGO_EMITTED_UI_ELF --list --color=never
group-ui-lib: ORIGINAL_CARGO_EMITTED_UI_ELF ALL_28_UI_FULL_NAMES --exact --format=pretty --color=never
```

The three test invocations use default scheduler policy, capture and exact multiple-name OR selection. Supply neither --test-threads nor --nocapture. Future names-only task admission should refuse an inherited RUST_TEST_THREADS or RUST_TEST_NOCAPTURE override; it must not read/clear/rewrite those values. This requests ordinary default scheduling, not proof of actual degree≥2/overlap. Groups and Cargo phases remain serial. No UI/HOST union or default/full feature graph.

## Reader, selection, parser and accounting seams for actualB

1. New task reader/admission: replace #2025 proof/schema/head/tree/source/helper/broad-filter literals with genuine eb7 and unchanged b914 source-review chain, new CLI reviewfcfa4, and a fixed job-kind discriminator A/B. Admit A exactly3 checks/0 artifacts/0 groups/3 phases; B exactly0 checks/3 artifacts/3 groups/9 phases. Bind new WT/controller/plan/review/archive hashes and the12 declared total phases across two distinct jobs. Preserve original source/context/runtime readers and result semantics; none of #2025 broad-helper preconditions or logical counts silently transfer.
2. Artifact/phase task seam: reuse unique successful original Cargo JSON root cagents libtest selection, target/path/ELF/hash checks. Per-artifact features must be exactly[]/[host]/[ui], never their union. For B, retain compile→inventory→one exact group per artifact. Do not reuse the #2025 fixed two groups (four-name plus broad substring) or run its broad filters. Adapt only this declared task loop/data and its phase labels.
3. Fresh selection task seam: from each original complete inventory derive ordinary set T and benchmark set B and N=|T|+|B|. Validate footer/cardinality/no duplicate names. Require requested exact set S⊆T once each, no matching benchmarks; K=|S| is1/1/28. Save group contract/full inventory before invoking that artifact. No cached/guessed N; the newly shared parent changes all three compiled inventories. If actualB cannot retain benchmark N correctly or take one exact group for each of three artifact groups, that is a concrete blocking task seam to fix/review, not permission to guess N or widen generic parsing.
4. Group parser: reuse actual qualified SourceB strict original-stdout parser. Require original exit0, one runningK record, every selected full name exactly once (unordered), one footer with passedK/failed0/ignored0/measured0/filteredN−K; refuse unknown/duplicate/missing/interleaved child/result records, unsupported progress syntax or extra footer. Keep raw stderr separate and retain failure output. No last-footer picking or borrowing narrow/other-feature passes.
5. Accounting/final task seam: A counts3 phase waits,0 test invocations,0 logical parents; B counts9 phase waits,3 test invocations and30 selected logical parents. Exactly30 distinct assigned parents across feature groups, not30 process waits. The five CLI cases/internal variants and UI assertions are nonadditive. Keep per-group membership/status, original process/wait/evidence facts and unique coverage separate. Final qualification requires all12 declared phases in their own jobs, all3 complete successful groups, both positive original returns and source/context admission. A pass alone, B partial success or safe lane return is not aggregate qualification.

ActualB source-review prerequisite includes typed/schema reader correctness, fixed phase order/caps, matching root manifests/features, exact selection membership, benchmark filtered-count algebra, group output corner cases and unchanged lifecycle/conservation tail. No generic cleanup or result weakening is part of this derivative.

## Exact selection and source proof

Module ancestry is immutable: lib.rs:5→modules.rs; modules.rs:33 unconditionally enrolls control,18-19 enrolls app only for ui,73-74 host only for host. control.rs:4454-4455 enrolls tests. The new shared parent is registered in CLI, HOST and UI inventories, but assigned execution is CLI only. Thus source-selected presence obligations: CLI contains CLI1 and excludes HOST1/UI28; HOST contains CLI1+HOST1 and excludes UI28; UI contains CLI1+UI28 and excludes HOST1. Registration in multiple inventories does not multiply assigned coverage.

CLI1: `control::tests::startup_state_uses_framed_proof_pinned_transport_for_variants_and_refusals` (`src/control.rs:5943`). It uses UnixStream::pair, scoped peer thread, real framed ControlClient/from_stream/startup_state and five Ready/Restoring/Retained/remote-refusal/foreign-proof cases. Five cases remain one outer parent; no daemon/autostart/native/helper/provider needed.

HOST1: `host::state::tests::startup_state_is_answered_while_startup_recovery_refuses_other_requests` (`src/host/state.rs:5037`). It constructs test_host→HostState::new (`state.rs:414`, in-memory fields) and calls actual process_request only for StartupState and BuildInfo. StartupState reducer arm is `host/client.rs:902`; recovery admission is rechecked at527-528. The exact body contains no upgrade_fixture_in_fresh_process, subprocess/current_exe/native restore/helper call. That distinct helper is state.rs:6337 and its callers6423/6446/6465/6505, outside S. Therefore the selected HOST startup-state-only group requires no #2025 broad-suite executable helper. Do not bootstrap or execute it as a qualification prerequisite.

UI28, unchanged from b914:

- `app::startup_gate::tests::retained_daemon_recovery_reports_its_reason_at_once_and_keeps_the_panes` (`src/app/startup_gate/tests.rs:118`)
- `app::startup_gate::tests::retained_reason_fits_one_launcher_handshake_line` (`src/app/startup_gate/tests.rs:156`)
- `app::startup_gate::tests::panes_without_progress_fail_before_the_inventory_budget_once_the_daemon_is_ready` (`src/app/startup_gate/tests.rs:171`)
- `app::startup_gate::tests::settling_panes_or_a_restoring_daemon_keep_the_gate_waiting` (`src/app/startup_gate/tests.rs:191`)
- `app::startup_gate::tests::the_inventory_budget_still_caps_a_daemon_that_keeps_restoring` (`src/app/startup_gate/tests.rs:229`)
- `app::startup_gate::tests::inspection_refuses_remote_before_resolving_an_ordinary_starting_bridge` (`src/app/startup_gate/tests.rs:254`)
- `app::startup_gate::tests::old_inspection_cannot_clear_a_new_ticket_or_change_its_progress` (`src/app/startup_gate/tests.rs:271`)
- `app::startup_gate::tests::current_connected_owner_refuses_old_or_incomplete_inspection_proofs` (`src/app/startup_gate/tests.rs:351`)
- `app::startup_gate::tests::losing_a_connected_owner_invalidates_cached_and_delayed_ready` (`src/app/startup_gate/tests.rs:417`)
- `app::startup_gate::tests::cached_ready_is_revalidated_after_a_new_owner_connects` (`src/app/startup_gate/tests.rs:454`)
- `app::startup_gate::tests::delayed_retained_cannot_override_current_readiness_or_save_completion` (`src/app/startup_gate/tests.rs:496`)
- `app::startup_gate::tests::a_retired_worker_keeps_occupancy_until_its_original_completion` (`src/app/startup_gate/tests.rs:615`)
- `app::startup_gate::tests::a_stale_inspection_does_not_hide_a_real_save_error` (`src/app/startup_gate/tests.rs:746`)
- `app::startup_gate::attachment_tests::hibernated_startup_waits_for_fresh_exact_ack_after_socket_eof` (`src/app/startup_gate/attachment_tests.rs:154`)
- `app::startup_gate::attachment_tests::another_resumed_terminal_cannot_hide_the_actual_attach_refusal` (`src/app/startup_gate/attachment_tests.rs:232`)
- `app::startup_gate::attachment_tests::oversized_terminal_refusal_fits_the_startup_handshake_without_losing_recovery` (`src/app/startup_gate/attachment_tests.rs:285`)
- `app::startup_gate::attachment_tests::hibernated_current_owner_fences_inspection_even_when_the_saved_proof_lags` (`src/app/startup_gate/attachment_tests.rs:349`)
- `app::startup_gate::attachment_tests::actual_reconnect_retires_cached_inspection_until_fresh_acceptance` (`src/app/startup_gate/attachment_tests.rs:425`)
- `app::shared_views::tests::startup_terminals::reopened_shared_window_qualifies_offscreen_terminals_without_opening_them` (`src/app/shared_views/tests/startup_terminals.rs:297`)
- `app::shared_views::tests::startup_terminals::reopened_shared_window_reports_a_refused_offscreen_attach` (`src/app/shared_views/tests/startup_terminals.rs:322`)
- `app::shared_views::tests::startup_terminals::shared_offscreen_terminal_waits_for_registry_admission_before_attaching` (`src/app/shared_views/tests/startup_terminals.rs:350`)
- `app::shared_views::tests::startup_terminals::a_replaced_shared_session_is_qualified_again` (`src/app/shared_views/tests/startup_terminals.rs:373`)
- `app::shared_views::tests::startup_terminals::a_detached_attach_is_retried_after_a_pause_and_then_reported` (`src/app/shared_views/tests/startup_terminals.rs:415`)
- `app::shared_views::tests::startup_terminals::a_shared_pane_attaches_its_exact_hosted_terminal_under_the_current_proof` (`src/app/shared_views/tests/startup_terminals.rs:449`)
- `app::shared_views::tests::startup_terminals::a_hibernated_offscreen_attachment_qualifies` (`src/app/shared_views/tests/startup_terminals.rs:481`)
- `app::shared_views::tests::startup_terminals::a_detached_offscreen_attach_is_reported_rather_than_awaited` (`src/app/shared_views/tests/startup_terminals.rs:493`)
- `app::shared_views::tests::startup_terminals::restored_observed_row_waits_for_its_own_seed_ack_after_connected` (`src/app/shared_views/tests/startup_terminals.rs:577`)
- `app::shared_views::tests::startup_terminals::a_queued_transfer_waits_for_target_ack_without_blocking_an_admitted_row` (`src/app/shared_views/tests/startup_terminals.rs:638`)

## Scope remaining after both jobs

Total focused outcome is30 parents, nonadditive. No whole suite/all-target/main/native/provider claim. Repository affected isolated all-target test coverage remains a separate requirement; this queue is the minimal focused qualification, not a waiver. Official format evidence and original b914 source evidence retain their own scopes. fff2 #2025 guards are still absent; do not borrow its test scope or results.

Native remains separately authorized: matched cagents/cagentsd/cagents-ui from actual qualified head via development runner, isolated private state and authorized private fresh daemon, exact initial/post-save/hibernated/refusal/queued-target recovery states, responsive polling, native before/after WebPs and independent UX review. No actual parallel-degree, provider or native outcome is established by these default-scheduler groups.

Exact source anchors/pins:

- `src/lib.rs` SHA256 `8694b000688068810d33e2a50d2f9973f987daf3dc34b14004843a388ce79d1e`
- `src/modules.rs` SHA256 `5fdc1d071a5263883d1415ffb8ec84435128aad1019ef3753924bd37a2599374`
- `src/control.rs` SHA256 `5ffc695ec8029d10808e05647d5bd106ff51e2e0b728b9a3b6ed3f2404e410c1`
- `src/host/mod.rs` SHA256 `8e8e16646cf8f964e5f736a68f1c42ec6e6feea32a5af5073afda696d9676864`
- `src/host/state.rs` SHA256 `76c4ac44cfa70751f1e473ebfcc6ca6abecf0b1af2cafb70220332bfc7bf7090`
- `src/host/client.rs` SHA256 `edbc47b6a792da070bc6d9bdc721bf378375c6098a7f9bc803f2f7d14f1638d7`
- `src/app.rs` SHA256 `22aa490df20e15110048cd9cdceaba2a365afff85c8bd442efb649f3b0ce92cf`
- `src/app/shared_views.rs` SHA256 `84f5c058963419c53f87435d1f9a3b0aa16fb048196d205a2e210458da9b3ffd`
- `src/app/startup_gate.rs` SHA256 `b3c219bd5f209c0d17b937ce231f613d7737abc45d2e7c581b4fd7786ed4332d`
- `src/app/startup_gate/tests.rs` SHA256 `0c87c715d782676cfec534600b6bb6b9fd0c9cbedcbe7da701b1ab7e2e6f2d6c`
- `src/app/startup_gate/attachment_tests.rs` SHA256 `ea53b6954256db7c61398c48d1009f31e148e18c6c9169b565e7f3a5653c4717`
- `src/app/shared_views/tests/startup_terminals.rs` SHA256 `0fa2cfaf9bd1a7a9cab447643f80cb03ea9439da77a67dba43b17a23345573bb`
- `src/terminal/ipc.rs` SHA256 `8d46e960e3d00fb35f111959ea6cbcf98f1ac6749cd715d5fe47636d1763f885`
- `Cargo.toml` SHA256 `8cf513d01300436076de1092e3c45cbbadc6c3c4b49286b65f882d7a3da92e48`
- `Cargo.lock` SHA256 `cbcb8b31eb6b00cbafe0a087145e2e6274ce5775413827b4f35295f0b93c383e`
- `.cargo/config.toml` SHA256 `2c342229ea06870a03fd22eede7299b883b2d5eb02d9f162a4f71b041c705018`
- `build.rs` SHA256 `9b97e52fdf299915e32838ec0bd8e60798adf9a37d19890422b4180e022372e9`

End verification: eb7 immutable parent/tree remain exact; all old14 non-control feature files still match b914, and new control insertion inverse remains byte-exact. Actual SourceB executable/controller qualification, C availability/grant, current context, actual artifacts/inventories, results and safe return remain unobserved/future. Only this additive report was written.
