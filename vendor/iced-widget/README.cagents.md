# Iced widget accessibility forwarding patch

Source: `iced/widget` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`, whose `iced` submodule is [pop-os/iced](https://github.com/pop-os/iced/tree/87c0358985cae50fefdcb15eabb0271e19ecf4a2/widget) `87c0358985cae50fefdcb15eabb0271e19ecf4a2`. The import is its own commit, unchanged; the upstream MIT license and `rustfmt.toml` come from the iced repository root. The manifest is normalized to retain the same pinned dependency graph and features outside the upstream workspace, and `build.rs` carries the checkout identity anchor that `scripts/qualify-checkout-cache.py` requires.

Cagents issue #1490: the runtime builds the accessibility tree only from `Widget::a11y_nodes`, whose default reports no nodes. A wrapper that does not forward it hides every control inside it from screen readers and AT-SPI tools, although the controls still draw, take input and receive focus through `operate`. These wrappers now forward their children's nodes:

- `row::Wrapping` (`row![..].wrap()`) and `column::Wrapping` delegate to their inner `Row` or `Column`. Wrapping only repositions children, so child layouts stay one-to-one.
- `keyed::Column` joins its children's nodes the way `Column` does.
- `Lazy` reports its cached element's nodes, as its `drag_destinations` already does.
- `Tooltip` reports its content's nodes. The tooltip itself is an overlay.
- `hover` (`Hover` in `helpers.rs`) reports both its base and its top layer, as `operate` visits both: keyboard focus reveals the hidden layer, so assistive technology must reach it too.
- `Table` joins its cells' nodes in row-major order, headers first, matching its child layouts.

Regression tests run against the real patched build from the cagents crate: `ui_controls::accessibility_probe::tests` lays out real buttons in each wrapper and checks their names. For the wrapping row and column it also checks that each button reports the bounds of its wrapped layout. `automation_ui::tests::every_automation_button_is_exposed_by_name` covers the automation pane that exposed the bug.

The same gap remains in wrappers cagents does not use: `Stack`, `Pin`, `Float`, `Sensor`, `Grid`, `List`, `Themer`, `Responsive`, `opaque` (`Opaque` in `helpers.rs`) and `PaneGrid`. `ui_controls::responsive` replaces `Responsive` for this reason. Remove this patch when the pinned upstream widgets forward equivalent nodes.

Outside this crate, libcosmic's `Dropdown` (`ui_controls::field::select`) and `Toggler` (`ui_controls::field::toggle`) report no accessibility nodes at all; they are follow-ups rather than part of this patch.
