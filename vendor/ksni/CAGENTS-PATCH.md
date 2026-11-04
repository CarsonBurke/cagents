# Local menu identity correction

This directory starts from the complete published `ksni` 0.3.6 crate archive.
The crates.io archive SHA-256 is
`814b44c24cd2cb236c3b8a41c7f08237b452a8e76ecaa81f1cec40b5b678215b`.
The original `UNLICENSE`, `Cargo.toml.orig`, published lockfile and VCS
provenance are preserved. The normalized `Cargo.toml` adds only an isolated
workspace with resolver 2 so package tests can run without joining the
application workspace. The Cargo cache is unchanged.

Local source changes add `StandardItem::key: Option<String>` (default `None`),
carry it into the internal raw item, and treat a changed positional key as a
layout change. The existing exhaustive integration-test initializer explicitly supplies
`key: None`, preserving its original unkeyed behavior. The existing ID-offset
mechanism then refuses old desktop IDs
instead of dispatching the new callback at the same position. Keys are local
identity metadata, never DBus properties or authority. They must remain stable
across label/enabled updates; unkeyed items retain upstream positional behavior.

The added service tests call the production `update_menu`, layout builder and
event dispatcher using an actual session-bus connection and menu interface.
They require an execution-owner-provided isolated session bus. They do not
register a desktop tray, launch a bus or establish native desktop acceptance.
All compilation and tests are unrun at this source checkpoint.

The containing private component worktree selects this crate through a root
path patch; its lockfile changes only the ksni registry source/checksum. This
is source integration, not a compiled application or native qualification.
