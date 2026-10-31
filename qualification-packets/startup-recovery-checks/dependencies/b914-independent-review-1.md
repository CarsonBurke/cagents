# Combined #2030 R1/R2 + #2029 V3 on main4429 — gate consumer source review

Verdict: BOUNDED ASSEMBLED SOURCE CLEAR in the assigned gate/consumer-join scope. No concrete unresolved source blocker found at the exact frozen bytes. This does not clear transport internals, authored test correctness, Rust execution, native UX or whole-feature/merge readiness.

ALL Rust compilation/tests and runtime/native execution are UNRUN.

## Exact reviewed source

Worktree:/home/marvin/Documents/repositories/cagents/.worktrees/fw16-startup-recovery-composition
Frozen aggregate is uncommitted over HEADac54c12bb54dc433366fe2eeb992faa348428acb.
Observed HEAD tree14e8a22e3945ab63f5fd1fd50b56fcb03e3ff3ff and parent27b6755a557e0225be863be322d63324a7d88152 identify the underlying committed composition, not a tree ID for the uncommitted aggregate.

Target main:4429728a1f179ea7f7eaca9667156553209a46ac
Inputs recorded by freeze:d2a0f4f8f873a75f462dd6d24d1a5ea2b0126b37 and78ee17d7f74a6238d12fd0dd38f74de4545e4fbd
Freeze:/tmp/fw16-backend-ec204421/startup-recovery-2030-2029-on-4429-source-v1/freeze.json
Manifest SHA256de41158a936a4086958ca95b126c621cf16b119c50870e25131bbe49df5727bc

All15 manifest file hashes/byte lengths matched both start and end. Gate SHA256 stayed b3c219bd5f209c0d17b937ce231f613d7737abc45d2e7c581b4fd7786ed4332d,28521 bytes,714 lines. No source mutation occurred during this review.

Prior plans used as the scoped acceptance contract:
- /tmp/pr2030-34917347-pr2029-78ee17d7-startup-gate-composition-review-v2.md, SHA2569ac50e33257daecd88c86d3be77b7103649b4dc5463f83f8e79120cdfd685ba9
- /tmp/pr2029-owner-basis-r1-current-attachment-minimal-plan.md, SHA25600a9f9665fd36a2b4a69d3bdcd53527825830b246216455c4aa3dcc75217e58d

This receipt supplements rather than overwrites earlier V2 negative and V3 closure records.

## Actual combined join matches the plans

### State/occupancy separation is preserved

State now contains the terminal Detached ledger alongside the V3 observation/ticket/context/outstanding fields (18-44). retire_inspections132-136 clears eligible inspection, cached daemon and Pending only. It neither erases Detached nor releases outstanding.

reserve418-445 still refuses a physical outstanding ticket, uses checked_add, captures owner basis/Pending/opened, and stamps dispatch time. Completion448-466 releases only its exact outstanding ticket before independently checking eligible context, gate instance, reported/saving phase and fresh Pending. An old completion cannot clear a newer worker or install a result after retirement.

The production inspection_task600-611 still returns the same immutable context after awaiting the actual blocking producer, including mapped producer/JoinError outcomes. No new cancellation or transport framework has been introduced. The unchanged remote unavailable boundary remains explicit; this review did not duplicate its transport audit.

### Every pending poll schedules qualification without bypassing its admission

The merged pending branch332-379 retains V3 current-cache validation, Pending progress, the absolute inventory budget, Ready-only no-progress inference and200ms poll. At378-379 it additionally schedules the2030 qualification Task in the same batch as poll and inspection.

qualify_startup_terminals290-310 retains the existing admission/candidate/may_attach/start-in-flight checks and dispatches through attach_next_restored_agent. This is no longer a begin-only qualification call, so late projection and pending attach retry have a scheduled path.

The app's actual dispatch still invokes shared_terminal_ready_for_restored_attach atapp.rs5994 before cold-row lifecycle mutation/attachment, retains deferred rows5995-5996/6015-6016, and starts only after that check. The helper is present in shared_views.rs360-392 and checks the target's unique canonical owner, initialization, seeding/blocking/retirement/pending-creation and source basis. The shared-view continuation remains at1353. This is a check that the reviewed R2 boundary survived composition, not a new independent R2 admission audit.

Relative to the underlying committed composition, app/shared_views.rs and saved_terminal.rs have no aggregate diff. app.rs changes are exactly the local StartupStateInspected variant4412 and dispatch16193; chat_ui.rs adds only the previously reviewed read-only connected-proof accessor. The2030 attach outcome still records terminal_detached atapp.rs14706 before finishing the serial start, so its ledger is actually populated.

### R1 readiness and current owner evidence are distinct and correct

The terminal scanner187-280 now calls terminal_surface_ready at212. None becomes Pending.panes at214; an API refusal/owner mismatch becomes a failure/reason at215-217. saved_terminal_unavailable and exhausted detached attempts still supply their meaningful reason. Ended/dormant terminal and retained-local-draft behavior is conserved; existing admission/other-pending precedence remains261-263.

terminal_surface_ready628-642 is byte-identical to frozen R1. It requires exact terminal ID, actual current attachment, complete proof and settled saved-owner equality before hibernation or an awake live grid can qualify. None stays pending; Err remains a refusal. Thus a hibernated detached visual marker does not prove disconnection, and an old visible grid cannot waive missing current attachment.

startup_connected_owners529-553 implements the exact00a9 hunk:
- open, non-ended, exact-ID terminal only;
- one current_attachment query, .ok().flatten(), complete-proof filter;
- no snapshot.detached, desired visibility or historical saved-proof equality as ownership evidence;
- actual connected chat getter unchanged.

Consequently a hibernated current B owner is present even if stored presentation briefly still says A. EOF/retry or definitive failure removes current evidence; error text is dropped only from this evidence vector, while the scanner retains it for the user. The collector does not perform migration, retarget a row or rewrite continuity.

The captured Vec<(CardId,fullproof)> remains immutable context39-44. startup_observation_current514-527 still compares the current vector with the captured vector and checks every current proof against the strict response proof, along with endpoint/opened/Pending. New, lost, mismatched or refused known owner invalidates the result/cache before stall use. Fresh empty/unknown-owner strict-RPC behavior and the declared limit for entirely unobserved same-unknown attempts remain unchanged.

### Initial, late-result and post-save paths remain joined

- Begin312-329 retires observation eligibility, preserves physical occupancy/terminal attempt ledger and schedules qualification.
- Fresh already-ready panes retire observations before saving at381-389.
- A delayed result rechecks current Pending at463-466; already-ready panes use ordinary verification/save rather than old Retained.
- A known-owner change rejects a successful old result at476; current cache is revalidated before Ready can time a stall at343-372.
- Failure568-573 retires eligibility and emits the existing recovery-incomplete notice/report.
- Post-save575-594 acknowledges actual settings, preserves actual save errors and reruns the same current readiness scanner. Pending returns through the merged verifier, including qualification and physical occupancy. There is no cached-Retained-over-success branch.
- A still-running retired worker stays outstanding across save->pending; it must complete its exact ticket before another inspection can reserve. Terminal qualification remains a separate task and is not gated by that inspection occupancy.

The following V3 function bodies were compared as exact source text and are unchanged: retire_inspections, inspect_daemon_startup, reserve_startup_inspection, startup_state_inspected, startup_observation_current, fail_startup_gate, startup_panes_saved, inspection_task, inspect_existing_daemon, stalled_reason and report. R1/2030 terminal_surface_ready, terminal_detached, detached, detached_reason, may_attach and qualify_startup_terminals also compare exactly to their frozen input bodies. These are static conservation checks, not Rust tests.

### Shared bounded summaries preserve useful failure reports

Terminal reason insertion267 calls startup_detail647-667. retained_reason671-676 now uses that same helper. It preserves the existing1024-byte body allowance plus at most a3-byte UTF-8 ellipsis, normalizes whitespace and keeps the recovery-incomplete prefix.

The terminal path caps printed pane IDs at12; the daemon Retained path has fixed surrounding prose. Those two dynamic-detail paths remain below the4096-byte launcher line limit with error prefix/newline. Raw reason values in their existing ledger/recovery state are not rewritten by the borrowed-string formatter. The unchanged workspace-save error String is outside this new dynamic-detail bound, as in the prior scoped review; no global error-size claim is made.

## Source-level usability and limits

The assembled source has a path to qualify sleeping shared/offscreen panes without opening or relaunching them, a path to report actual attachment refusal with recovery instructions, and a fresh post-save check. It preserves meaningful current errors instead of substituting obsolete inspection detail. No UI controls or user-start body were removed by the aggregate app changes inspected here.

This is source-level outcome reasoning only. No actual native window, focus/scroll/hover, representative scale, latency, provider, migration or recovery action was exercised. Current-attachment producer truth is assumed from the separately reviewed frozen R1 implementation; its transport state transitions were not re-audited. R2 target correctness and all detailed fixture/evidence assessments remain separately owned.

All test source files were pinned but not substantively re-reviewed. The manifest's28 authored parents and format/diff metadata are not independent passing receipts; original Rust/native evidence is still missing. The actual combined candidate needs those checks and native before/after/UX evidence before feature readiness or merge claims.

No edits, builds/tests/imports, runtime/native/endpoint/process/account/DB probes, remote/public writes, lane/C use or new workers occurred. Only read-only source/metadata/hash comparisons and this additive /tmp report were produced.

## Start/end pin inventory

| File | Bytes | SHA256 |
| --- | ---: | --- |
| src/app.rs | 1380000 | 22aa490df20e15110048cd9cdceaba2a365afff85c8bd442efb649f3b0ce92cf |
| src/app/chat_ui.rs | 1214271 | 89157262169923105ad2d63ef79cda6a4fc76c9cd3b6ba726d2f601a1028dd84 |
| src/app/saved_terminal.rs | 37956 | 0703f9cc4f8255d6c8896394575f8177ec88e4ef9b09bc005f6939e05df7a0bd |
| src/app/shared_views.rs | 233376 | 84f5c058963419c53f87435d1f9a3b0aa16fb048196d205a2e210458da9b3ffd |
| src/app/shared_views/tests/startup_terminals.rs | 29571 | 0fa2cfaf9bd1a7a9cab447643f80cb03ea9439da77a67dba43b17a23345573bb |
| src/app/startup_gate.rs | 28521 | b3c219bd5f209c0d17b937ce231f613d7737abc45d2e7c581b4fd7786ed4332d |
| src/app/startup_gate/attachment_tests.rs | 18278 | ea53b6954256db7c61398c48d1009f31e148e18c6c9169b565e7f3a5653c4717 |
| src/app/startup_gate/tests.rs | 30317 | 0c87c715d782676cfec534600b6bb6b9fd0c9cbedcbe7da701b1ab7e2e6f2d6c |
| src/control.rs | 299253 | 37c489822925d1e5a62fe6a87aee2e226379979ae399f34957065277f948a3e9 |
| src/host/agent_execution/terminals.rs | 9314 | 2544087dae9936d68112512aa3263baf687a4bc05eb02a99af12f743fd68bbdf |
| src/host/client.rs | 155313 | edbc47b6a792da070bc6d9bdc721bf378375c6098a7f9bc803f2f7d14f1638d7 |
| src/host/state.rs | 336801 | 76c4ac44cfa70751f1e473ebfcc6ca6abecf0b1af2cafb70220332bfc7bf7090 |
| src/terminal/hosted.rs | 186517 | 4a97e21a38a90efae530c9f35de48f633ff0afda2756ef8d006f74012a95ecc8 |
| src/terminal/ipc.rs | 113283 | 8d46e960e3d00fb35f111959ea6cbcf98f1ac6749cd715d5fe47636d1763f885 |
| src/terminal/surface.rs | 6261 | da134f33421603317eb7183574147879e227b33d863756e081fa8588b51f4d21 |
