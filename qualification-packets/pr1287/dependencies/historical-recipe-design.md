# PR1287 terminal-continuity P2: finite fix design

Recommend one pure guard in `closed_pane_matches`: reject a RelaunchAvailable nested recipe that differs from the **proved original console's launch** before the existing ended-continuity normalization. Keep the full-descriptor comparison and all owner/proof/thread checks. This design makes no source change.

Pins: published PR1287 head `88dbd65859be8b09e5664d2d10cbec5059fddd0b`, tree `386694df215f9456654c618c653dcd67f78ac8a6`; separately frozen root correction `015e222de6a8852eeddbd221e3822f0e6fad963b`, tree `7c07e65850f0b0c2efd473fe42f172e6d1fe39a5`. Actual GitHub main observed through its branch GET is `e8842819519dff74c0726d1fdd94f435b495a153`, tree `c94d47bbe91063e1790380e0522ac8a9d03d8057`. Published metadata observed 2026-10-30 15:10:56 UTC remains OPEN/DRAFT at the exact original head. No changes were made to the frozen worktree.

## Published constraint and causal gap

The two published continuity reviews (2026-10-28 20:10:05 UTC and 21:05:33 UTC; review IDs `PRR_kwDOUFbDEM8AAAABUxeaqw` and `PRR_kwDOUFbDEM8AAAABUyGu3Q`) require exact nested recipe equality before exit normalization and program/cwd/environment controls. They explicitly classify this as a new qualification-API defect: neither inspector has a production caller at that head. No live upgrade bypass is established.

At [original archive.rs:184](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/automations/archive.rs#L184), the predicate copies the entire current Exited or RelaunchAvailable(Some exit) continuity into the cloned issued console, then compares the full TerminalSession. A caller can keep every outer field equal while changing only nested recipe and still qualify. Raw `read_migration_session_at` calls `load_terminal_record` ([persistence.rs:4774](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/persistence.rs#L4774)), bypassing ordinary `load_terminal` restore canonicalization ([4758](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/persistence.rs#L4758), [4936](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/persistence.rs#L4936)). The fix must refuse the mismatch rather than rewrite the untrusted nested recipe.

## Established recipe contract

- The console is a separately issued terminal. [create_console:1040](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/host/agent_execution.rs#L1040) constructs the matched CLI executable with `agent console <agent>`, provider working directory and empty explicit environment/removals. It writes this recipe into both terminal.launch and initial nested recipe at lines1063-1068, then publishes the actual spawned descriptor into the native pair/registry at lines1088-1134. It is not the provider executable recipe.
- The actual ended projection [app.rs:1534](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/app.rs#L1534) copies `session.launch` into RelaunchAvailable.recipe and adds Some(last_exit). Thus for this retained original console the legitimate ended nested recipe is its original.launch.
- A user relaunch may choose an agent resume recipe, but [fresh_terminal_relaunch:1548](https://github.com/CarsonBurke/cagents/blob/88dbd65859be8b09e5664d2d10cbec5059fddd0b/src/app.rs#L1548) creates a fresh terminal ID/predecessor and sets the new outer launch and nested recipe together. That fresh identity is not an alternate allowable recipe for the old archive console.
- Current-main [closed_channel_source_matches:238](https://github.com/CarsonBurke/cagents/blob/e8842819519dff74c0726d1fdd94f435b495a153/src/app/channel_ui.rs#L238) refuses nested recipe != before.launch before normalizing the ended continuity. Current-main [host/model/views.rs:65](https://github.com/CarsonBurke/cagents/blob/e8842819519dff74c0726d1fdd94f435b495a153/src/host/model/views.rs#L65) accepts RelaunchAvailable(Some exit) as ended only when recipe == session.launch. These corroborate the original producer contract; no source is imported from either helper.

## Minimal correction

Immediately after the existing original/native ID and complete-proof checks (original lines177-183; frozen correction lines196-202), before `let mut original = original.clone()`, add this semantic guard using the existing fully qualified type:

```rust
if let crate::model::TerminalContinuity::RelaunchAvailable { recipe, .. } =
    &current.continuity
    && recipe != &original.launch
{
    return false;
}
```

Then retain the existing clone, the Exited / RelaunchAvailable(last_exit: Some(_)) whitelist, continuity assignment and final full equality.

This preserves exact original live-console qualification, legitimate Exited and matching-recipe RelaunchAvailable(Some exit), and accepted exit status/time observations. RelaunchAvailable(None), Failed, changed live/detached proof, ID, outer recipe, listener, recovery/predecessor, transcript path or resume hint retain their current equality/refusal behavior. No native thread/proof/owner condition is loosened: `qualify_kind` still calls `qualify` on the canonical original native session, which requires closed Automation ownership, full native descriptor, complete proof and the canonical confirmed nonblank snapshot thread. Compare the complete LaunchRecipe, including arguments and unset_environment, not only its program/cwd/env subset.

Do not compare against `native.launch.recipe`, generate an agent resume recipe, derive a newer companion executable, repair the current descriptor, or add provider/startup/IO behavior. No shared helper extraction is required for this one predicate.

## Focused owning test parents to add later

1. **Pure qualification parent beside `archive_receipt_requires_exact_closed_read_only_history` (original archive.rs453):** use its closed canonical owner/thread fixture plus an issued Live console with complete matching proof and CLI-console launch deliberately different from the provider recipe. Check `ArchivedPaneReadiness::qualify_kind` and `matches_terminal` for the exact Live console, Exited, and RelaunchAvailable(Some exit, original console launch). Change only nested program, arguments, cwd, environment and unset_environment one at a time while leaving the outer launch and every owner field fixed; each must error. Include nested provider recipe as a negative to detect choosing the wrong expected recipe. Keep None-exit, incomplete/changed proof, wrong snapshot thread and changed outer descriptor negatives, and a clean/failed/signalled ended control. Assert the original archive/console remains unchanged.
2. **Actual normalized-row inspector parent extending the terminal portion of `source_archive_inspection_is_read_only_and_needs_no_service` (original archive.rs645-768):** retain its real StateStore-created Terminal rows with NULL content_json and exact canonical registry record; add a valid matching-recipe RelaunchAvailable(Some exit) control. In the test fixture, alter only normalized `continuity_json` to contain each nested recipe mismatch, and pass that exact malformed raw descriptor to `inspect_completed_automation_pane`. Outer recipe_json and immutable registry owner remain untouched. Require rejection, then restore the valid raw continuity and require the matching terminal receipt. Compare source bytes before/after each inspection. Do not load through ordinary canonicalizing restore before the oracle, or the corruption disappears and the original defect is missed.

The first parent owns the pure contract; the second proves the real raw normalized decoder reaches it. These are authored-test recommendations only: no fixture, database, test, build, provider or native execution occurred.

## Source bindings (SHA256 of exact Git object bytes)

| Commit | Path | SHA256 |
| --- | --- | --- |
| 88dbd65859be8b09e5664d2d10cbec5059fddd0b | src/automations/archive.rs | da83916d91681f22b110b410c95910b3a68eecd7dcffaa392ad0aaed6b0a30ef |
| 015e222de6a8852eeddbd221e3822f0e6fad963b | src/automations/archive.rs | f420e2c88a37dcac288d2c145c74fe6a6249c9d713a50776729e5988a03778bd |
| 88dbd65859be8b09e5664d2d10cbec5059fddd0b | src/host/agent_execution.rs | da675da645f01eae7d4cd236d26bf14f6bf20972fdc4fc7301ba2b2d8e325f3e |
| 88dbd65859be8b09e5664d2d10cbec5059fddd0b | src/app.rs | de54092d75b54f35893da1217264c637902284dfb858479746b3f7187570f895 |
| 88dbd65859be8b09e5664d2d10cbec5059fddd0b | src/persistence.rs | 4c828bbcbec5e86e3332a03c3488de2efa6765ecbddc8a4607ca33ef30d23db7 |
| 88dbd65859be8b09e5664d2d10cbec5059fddd0b | src/model.rs | 63c3cfe7ef79fe8bbceb5d38d213ac9cd41238857100ef07d71ab6faba08ba7e |
| e8842819519dff74c0726d1fdd94f435b495a153 | src/app/channel_ui.rs | e634530abcf68d9d94c97af8705c07a934c485a8abf191499ea1d6cb052b9598 |
| e8842819519dff74c0726d1fdd94f435b495a153 | src/host/model/views.rs | 4fcde53fc87e36eb0e73518985edab78efb2aa43c2d483840552df8fc741e946 |
| e8842819519dff74c0726d1fdd94f435b495a153 | src/app.rs | c405ea52503acc5076a5e77d3bc81d5e9ab341b1a78e313147e9ca70dfc67e77 |

Scope excludes the separate bounded-workspace correction, UUID-alias finding, schema/WHOLE composition, provider/launch changes and production caller wiring. Existing draft/composition/native qualification gates remain unchanged. Only Git-object/GitHub source and metadata reads plus this task-owned /tmp report were performed; no source edits, Git fetch/write, builds/tests/rustc, runtime/database/process/state access, native/provider execution, GitHub writes, inbox/meters, children/new sessions or model/account switches. Finite design task complete.

