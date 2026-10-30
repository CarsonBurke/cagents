# Issue2015 validation command DATA: independent source review

Result: **selectors, feature gates and predicted integration inventories are source-correct; original 5a4 validation expectation had a concrete stale-assertion blocker. Root's separately frozen cause/guard correction is bounded-source-clear. Repin command DATA to the actual corrected commit before execution or any pass claim.** All execution remains UNRUN. No controller/lane grant follows from this receipt.

## Identities and preserved finding

Reviewed command DATA:

- `/tmp/issue2015-5a4d-validation-command-data-2h0z_rds/command-data.json`
- SHA256 `feb23120adabb815ba07c657d0eade56499a78fa38fcd5f264b6c5fdb80e80db`.
- Historical head `5a4d6e6278714f673d0ce6f9c8453007a417f3b2`, tree `274283405bb0bf92fa8fa621cc5393048af4ef4e`, parent `e8842819519dff74c0726d1fdd94f435b495a153`.

All seventeen listed Git blob IDs match that immutable tree. The relevant file-level gates, test names and source declarations were read. This is not verification of a complete dependency closure or live runtime context.

**Concrete original blocker:** historical `tests/installer_guard_contract.rs:918–922` requires `(os error 2)` in the `inspector-missing` inline case of `external_installer_saves_closes_and_reopens_windows_even_on_failure`. Historical `src/update.rs:1181–1183` instead maps the metadata error to a friendly message while discarding the OS cause. If the current historical CLI reaches this case, its diagnostic cannot satisfy that assertion. The command DATA repeats the obsolete “actual not-found cause” expectation. This is a source-confirmed qualification/test-oracle inconsistency, not an executed failing test. I missed this neighbor assertion in the earlier three-leaf source review; that prior bounded-clear receipt must not be treated as proof that the owning test can pass unchanged.

Root then supplied `/tmp/fw16-backend-ec204421/issue2015-cause-guard-correction-source-freeze.json`, SHA256 `9b3bb550982922098cb15695a60a30251374eadbf52ec22c421c70901f59ab33`. Its two-leaf correction retains `Cause: {error}` in both before/after metadata refusals and strengthens the missing-helper test to require the friendly unavailable/windows-usable/reinstall guidance **and** actual `(os error 2)`. Prepare/cancel ordering, held-window/fence conservation and every helper identity/proof gate are unchanged. No permissive fallback is introduced. **Bounded source clear for this correction.**

| Corrected frozen leaf | SHA256 |
| --- | --- |
| `install.sh` (unchanged) | `7b49493d90d0aeeb627b488c8c4fd372b005a038af83098860780b19c0134d07` |
| `src/update.rs` | `48a9c75b73a3948063f4fca76e0ecf3159e0065a3a9ab19d6d8749a8446eec55` |
| `tests/installer_lifecycle_contract.rs` (unchanged) | `ca9bb7034feddd8f21f303dea1ebcab2125315d8bf66a52964fe66df4fe05dec` |
| `tests/installer_guard_contract.rs` | `a4413f4a235ef796438d32fa538f0e1d46fa5221283a588e1f36aaac01d92a00` |

Both JSON hashes and all four corrected leaf hashes were checked again after inspection and remain exact. This receipt binds the correction to that freeze, not to a nonexistent corrected commit. Old 5a4 source pins cannot qualify execution of the corrected leaves; the owner must publish/review new command DATA after the actual commit. No format result is inferred from the still-running owner check.

## Gates, selectors and counts

- Both integration files have only a Linux top-level cfg. Neither is a Cargo `required-features=ui/host` target. `Cargo.toml:101–126` exposes the CLI without feature prerequisites, while UI and daemon binaries require their separate features. `--no-default-features --test installer_lifecycle_contract --test installer_guard_contract` therefore selects the owning CLI integration graphs without a HOST daemon build. Their actual CLI prerequisite comes from `CARGO_BIN_EXE_cagents`.
- Immutable lifecycle source contains **36** `#[test]` declarations, matching the complete name set in DATA; no ignored declarations. A singleton exact ordinary parent predicts 1 passed / 0 failed / 0 ignored / 0 measured / **35 filtered out**, subject to actual current harness inventory.
- Immutable guard source contains **11** `#[test]` declarations, matching DATA. One is the ignored `coordinator_fixture::guardian`. `staged_window_lease_fixture` and `staged_native_ui_owner_fixture` are passive helper entrypoints, not extra owning passes. A selected ordinary singleton predicts 1/0/0/0 and **10 filtered out**. “10 ordinary active source rows” is not a claim of ten distinct behavioral owning tests; two rows are passive helpers.
- The five core selected parent names and line anchors are exact: two new archive parents, unsupported coordinators, external installer/window/refusal parent, and inherited-fence parent. Five distinct selected owning parents is correct accounting. Each new archive parent has success/refusal inline cases, and the external parent has seven inline statuses; none becomes an additive pass. Inventory, compilation, guardian/lease helpers and CLI subprocesses are also nonadditive.
- `--exact` is correctly passed to libtest after `--`; zero-selection success must be rejected. Full inventory remains mandatory, rather than deriving runtime N solely from lexical declarations. The correction adds no tests and changes no selectors, so source predictions remain 36/11; still recheck on the final committed harness.

No inflated parent count or claimed completed run was found in the DATA. `actual_completed: 0` and UNRUN labels are appropriate. The stale negative assertion, not test-count inflation, was the concrete defect.

## UI scope and cheapest sufficient boundary

The five integration parents plus CLI check/compile cover the narrow installer regression and existing negative coordination/fence controls. They do not require HOST, UI or a browser implementation. The inspector is an explicit Python boundary fixture; lack of a HOST build is intentional, not evidence that a missing daemon is safe in production.

The shared guard `instance::tests::source_inspection_veto_cancels_all_prepared_windows_before_commit` is unconditional within the library's `cfg(test)` module (`instance.rs:2540`), and `instance` is an unconditional module. If additional direct preflight-veto conservation evidence is wanted, it can be selected through **CLI-only `--no-default-features --lib`**, with its own complete library inventory and N; no UI compile is needed for it.

The exact reducer parent `app::update_ui::tests::a_superseded_window_reopens_only_when_nothing_holds_it_open` genuinely requires **`--features ui --lib`**: `app` is UI-gated and the UI binary has `test=false`. DATA names the correct target. It is optional separate UI-handler qualification, not a hidden prerequisite for the new CLI archive tests. A CLI-only zero-selection result cannot qualify it. Do not promote the full optional UI command bundle into the minimum high-priority installer packet; no UI layout/feature/dependency change was made. If Root independently requires that exact reducer control, its UI-library compilation/inventory cost is real, and its task-dropping reducer test still does not execute native windows or the helper. No integrated-browser or HOST feature union is warranted here.

## Minimum actual runtime packet, not yet present

After the corrected source is actually committed and DATA repinned, a narrowly sufficient **installer-regression** packet consists of:

1. Final head/tree and relevant source pins, original argv/cwd/toolchain/features and actual source/artifact identities before/after; separately admitted ownership/custody and settled owned-family evidence. DATA is not allocation, and Main15's C ownership must not be transferred or assumed by it.
2. Original successful CLI check and the two selected integration-harness compile results. Retain compiler-artifact paths for the CLI and both harnesses, actual Running harness headers and original exits. Source pins or a header alone do not establish process/ELF identity or settlement.
3. Actual complete same-feature inventories for both compiled integration harnesses. Require the expected exact 36/11 sets, then record each of the five named singleton parent outcomes with 1 selected test, its named `ok`, 1P/0F/0I/0M, correct N−1 and original exit 0. Any discrepancy or source change invalidates the old count prediction.
4. Preserve the owning test source/hash and original output so the successful parent is attributable to its real same-archive coordinator/inspector hash/mode/fence/refusal assertions. The external negative parent must exercise the corrected friendly-and-cause assertion plus prepare/cancel and window-retention controls, not count missing-helper refusal as a fresh-install success.
5. Explicit limits: local archive/manifest fixtures exercise the actual CLI coordinator but controlled inspection results; they do not establish actual HOST source recovery, production daemon startup, live-provider behavior, native save/close/reopen UX or performance. A claim that a real matched release fully installs/starts requires its own isolated actual-archive outcome; no extra HOST build is demanded merely to run these labeled fixtures.

Natural caller authority is an execution precondition, not an assumed empty state. `install.sh:100–102` refuses any inherited `CAGENTS_AGENT_AUTHORITY`; the new lifecycle fixtures customize private HOME/XDG but do not remove that authority marker. If the admitted natural caller is delegated, expect the guard to refuse before archive work, rather than call such a run a layout failure or strip authority in a controller. This task did not inspect the live environment or establish which caller context a future lane will have. Source-owned explicit User fixtures, if needed, require the owning source review; the command DATA itself is not such a wrapper.

This review performed only immutable source/metadata/receipt reads, source hashes and this report write. No Cargo/compiler/inventory/test/helper/controller/native/install execution, edits, probes, inbox/GitHub operations, new workers or lane actions occurred. Stop after the finite source review; every runtime item above remains UNRUN.
