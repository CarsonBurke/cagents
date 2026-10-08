# Iced keyed child reconciliation patch

Source: `iced/core` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/core), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The manifest preserves the pinned dependency graph outside its upstream workspace.

`Tree::diff_children_custom` now assembles reconciled children in widget order. The pinned implementation diffs named children at their old indices and truncates before matching. Reordering a named button across an unnamed spacer leaves the spacer's childless tree under the button, which panics during layout. Reordering, inserting, and removing children must retain each surviving named tree at its new index. Unnamed children retain positional matching; repeated names are consumed in order instead of overwriting a prior tree. Custom names must still be unique within a native frame because the runtime named-state cache is keyed by name.

Regression coverage includes core tree state continuity and a native `UserInterface` rebuild of mixed named buttons and spacers in `src/interaction.rs`. Remove this patch when the pinned upstream core provides equivalent reconciliation.
