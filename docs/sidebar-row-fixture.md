# Sidebar row qualification source

This is an inert development-only fixture for issue #2004. It renders the
existing application sidebar, context menus, and confirmation dialogs. It does
not restore a workspace, open a database, admit a hosted session, execute an
editor, contact a provider, or grant native/operator authority. Inherited
authority stays unchanged. Native admission and operation still require their
normal authorized owner.

The product before revision is main
`6d1dbb59a96d22724817985039218ed25b535ed1`; the product after revision is
`8ca25f7c069aee117e7b2cd330b38eb98cca3029`. New qualification branches add the
same fixture and common development-only startup patch to each product base.
Qualification heads are different from product heads and must be recorded
separately. Do not reuse the older sidebar harness branches or their evidence.
The separate `da517` to `8ca` component comparison explains popup identity; it
does not replace this whole-feature comparison.

Common startup-isolation provenance is 066's frozen, independently reviewed
`af224df26e92f331812afddf2d762e97d2c8ee59`. Only its `src/app.rs` and
`src/bin/cagents-ui.rs` changes are reused: the predicate is consistently named
`native_validation_fixture_enabled`, leaf calls target `sidebar_fixture`, and
the chronology-only Tick enum and no-op match hunks are omitted. There is no
chat-client fixture or combined dispatcher. Both qualification branches have
identical startup files and identical latched fixture selection. Ordinary
production startup follows its original guards. This provenance transfers
source structure, not another lane's compile, native or operator evidence.

The source step has not built or launched these qualification heads. Cargo,
native input, display probes, helpers, screenshots, deletion execution, and
native UX review are all unrun. Source review cannot supply those results.

## Entry and data

Only a build with `development` can select this fixture. The exact environment
selection is `CAGENTS_SIDEBAR_ROW_FIXTURE=sidebar-row-2004`, or
`sidebar-row-2004-compact` for the same data starting in the compact rail. Use
the ordinary `scripts/run-development --browserless` path after the paired
source has been reviewed and a build/native turn has actually been assigned.
Keep the inherited account, authority, display and process boundaries. The
fixture does not require changing HOME, a saved database or account settings.
Use the established isolated development profile and an owned display; it is
not an instruction to replace an existing development session.

Both modes contain the same fixed original project (ID 1), primary worktree
(ID 2), merged original target (ID 3), ordinary unmerged neighbor (ID 4), four
similar-prefix channels, and six inert editor menu descriptions. Working,
waiting, failed-unread and quiet channel states are synthetic. The active
original worktree is `/offline-sidebar-fixture/merged-original`, branch
`fix/sidebar-identifying-title-original`. Its merged PR is deliberately
synthetic (`example.invalid`); eligibility discovery from Git or GitHub is
not exercised. No executable or provider is registered.

Physical F8 inserts worktree 90 before original worktree 3 and a separate
project 99 before project 1, once per launch. The new worktree has waiting
activity so it is visible in the compact rail. F8 changes the model only; it
does not dismiss/reopen the popup, replace sidebar state/cache, click a menu
entry, or emit Delete. The pinned COSMIC context menu closes on Escape or a
pointer press, not on F8. Its native behavior must still be observed at runtime.

Physical F9 switches to one empty synthetic project, for an empty-state frame.
Restart the owned fixture normally to reset its data. These two data controls
are fixture-only. Never substitute them for ordinary popup scrolling or row
actions.

## Bounded native interaction plan

Keep before/after viewport, scale, theme and data identical. Start at expanded
sidebar width 260, then use its real resize handle to inspect the supported
narrow and wide widths (148 and 380). For compact tall-menu checks use a short
window, for example 900 by 640 logical pixels, and record actual measured
bounds. Do not force a window below the application's normal minimum size.

1. Capture resting, row-hover, action-hover and selected similar-prefix
   channel and merged-worktree rows. Compare visible title and status bounds
   across rest to hover. Check the full action target, flat selection surface,
   primary/ordinary worktree eligibility, and compact activity indicators.
2. Right-click original worktree 3 in the compact rail. Keep all six editor
   entries. Record the tall popup before scrolling. Press physical F8 while
   that same popup remains open; record the unrelated project and neighbor
   insertion. If the popup closes or loses capture, record the failure; do not
   reopen it and count the restarted flow as success.
3. Move the pointer into the actual painted popup, wheel through the existing
   menu until its final Delete is visible, then physically activate Delete.
   No final action message or direct button call may replace the input. The
   real confirmation must name original worktree 3 and its original path,
   never inserted worktree 90 or project 99. Capture that confirmation.
4. Click Cancel, repeat and use physical Escape to cancel, and inspect focus.
   The unchanged storage guard makes the destructive confirmation disabled
   because `session` is None. This validates target discovery, the real
   guarded confirmation and Cancel only. It cannot qualify actual deletion,
   failure/retry, provider closing, persistence, or successful operation.
5. In expanded and compact modes, physically Tab and Shift-Tab through controls
   and inspect visible focus. Use real row Arrow/Home/End/Enter/Delete keys;
   check modal blocking and Cancel. Exercise press-on-row/release-on-action and
   the reverse, followed by a fresh same-control click. The fixture permits
   selection and local navigation presentation; it never opens a repository,
   channel runtime, external editor or link. Treat runtime navigation as untested.
6. Use the nonempty channel's ordinary quick Delete and cancel its real
   confirmation. Channel confirmation is displayed but ConfirmDelete is
   deliberately inert. Do not count its click as a successful deletion.
7. Capture F9's empty project, narrow rows, and ordinary modal interaction.
   The fixture does not provide concurrent provider traffic, long-history
   rendering, or resource/stall evidence. Those remain separate checks.

Store actual native before/after WebP pairs on a separate retained screenshot
branch, never this fixture or product branch. Identify both product revisions,
qualification heads, matched executable hashes, measured viewport/scale/theme,
and which real input produced each state. The PR uses only two image columns,
Before and After, with the state description beneath each pair. Component
frames, mockups, or this plan are not native screenshot evidence.

Give the actual images, input trace, target observations and tested head to
the existing independent reviewer 603. Resolve UX blockers and missing native
evidence before calling the interaction ready or merging. The earlier PR-write
approval rejection remains unchanged; this source step does not retry it.
