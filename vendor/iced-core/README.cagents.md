# Iced keyed child reconciliation patch

Source: `iced/core` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/core), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The manifest preserves the pinned dependency graph outside its upstream workspace.

`Tree::diff_children_custom` now assembles reconciled children in widget order. The pinned implementation diffs named children at their old indices and truncates before matching. Reordering a named button across an unnamed spacer leaves the spacer's childless tree under the button, which panics during layout. Reordering, inserting, and removing children must retain each surviving named tree at its new index. Unnamed children retain positional matching; repeated names are consumed in order instead of overwriting a prior tree. Custom names must still be unique within a native frame because the runtime named-state cache is keyed by name.

Regression coverage includes core tree state continuity and a native `UserInterface` rebuild of mixed named buttons and spacers in `src/interaction.rs`. Remove this patch when the pinned upstream core provides equivalent reconciliation.

Run the core regressions as a standalone package:

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml reconciliation_tests
```

Cargo cannot run these dev-dependency tests with `-p iced_core` from the parent workspace because this vendored package is not a workspace member. Its standalone lockfile and build artifacts are local test output.

## Explicit click timestamps

`mouse::Click::new_at` accepts an explicit timestamp so gesture fixtures can
exercise real event dispatch without depending on test execution speed.
`Click::new` still uses `Instant::now`; the existing consecutive-click policy
is unchanged, including the inclusive 300 ms deadline, position tolerance,
button matching, and single/double/triple progression.

Run the focused standalone timing regression with:

```sh
cargo test --manifest-path vendor/iced-core/Cargo.toml \
  mouse::click::tests::consecutive_clicks_include_the_deadline_and_reset_after_it
```

## Custom ID hashing

Custom widget IDs now hash their names, matching their existing equality comparison. Regression tests cover rebuilt named IDs, unique IDs, and sets. This implementation is selected without accessibility; the corresponding accessibility ID implementation is patched in `vendor/iced-accessibility`.

## Drag destination registration order

`DndDestinationRectangles::into_rectangles` retains insertion order for internal
widget traversal. Scrollable and scale wrappers extract, transform, and append
these lists; reversing on each extraction made overlapping target priority
depend on the number of enclosing wrappers. The patched Winit consumer uses
`into_native_rectangles` to reverse once when registering the final native list.
App regressions cover plain, nested surface, real Scrollable, file-drop scaling,
and combined wrappers with a preceding root sibling.
