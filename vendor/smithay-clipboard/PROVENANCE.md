# smithay-clipboard input-removal fix

Base: [pop-os/smithay-clipboard](https://github.com/pop-os/smithay-clipboard)
commit
[`859b02c88f45c554049a67c6ddeec1692ce0e20b`](https://github.com/pop-os/smithay-clipboard/tree/859b02c88f45c554049a67c6ddeec1692ce0e20b),
the tag `sctk-0.20` that `clipboard_wayland` (pop-os/window_clipboard, tag
`sctk-0.20`) depends on. `src/`, `Cargo.toml`, `LICENSE`, `README.md`,
`CHANGELOG.md` and `rustfmt.toml` were imported unchanged from `git archive` of
that commit. The import is its own commit, so `git log -p` shows this patch
separately. The CI workflows, the example, `doc_index.html`, `.gitignore`
and `rust-toolchain` are omitted. That toolchain file would select nightly
for Cargo commands run here.

## Fix

When a compositor removes a seat's last pointer, `remove_capability` releases
the `wl_pointer`. Events the compositor sent before it saw the release can
already be in the worker's Rust event queue. wayland-client rebuilds the
proxy for each of these events. Because the object is dead, that proxy has no
user data and reports version 0. sctk then calls `pointer_frame` for a queued
frame that carries pending events. Because version 0 predates frames, sctk also
calls it immediately for any other pointer event once a surface was entered.
`pointer_frame` unwrapped `pointer.data::<PointerData>()` and panicked. The panic stopped the worker
thread, so every later Copy was silently dropped.

Only `src/state.rs` and `src/lib.rs` differ from the base:

- `pointer_frame` attributes events to the seat whose current pointer sent
  them. Events from a released pointer belong to no seat and are ignored.
- The keyboard handler likewise ignores events from a keyboard that is no
  longer its seat's keyboard. A queued enter therefore can't restore focus
  after the keyboard was released.
- Removing the keyboard capability clears the seat's focus and serial, as a
  leave event does. A replacement keyboard therefore authorizes Copy only
  after the compositor sends it an enter, as the protocol intends. A pointer
  serial taken just before the keyboard's removal is also cleared, which is
  the same trade-off upstream's leave handling makes.
- `new_capability` and `remove_capability` ignore a seat that is already
  removed. Before, a capability change queued after the seat's removal
  panicked.
- `store_selection` returns `None` when the seat has no selection device. The
  focus changes above already make that state unreachable.
- `lib.rs` keeps the cagents checkout identity anchor (see
  `scripts/qualify-checkout-cache.py`).

## Manifest and tests

`.gitignore` ignores this package's standalone build output and lockfile.
`Cargo.toml` has an empty `[workspace]`, so Cargo can test this package
here without making it a cagents workspace member. Its example-only
dev-dependencies (`dirs`, `thiserror`, `url`, sctk with `xkbcommon`) are
replaced by `wayland-server`. Library dependencies and features are unchanged.

`tests/seat_capabilities.rs` connects an in-process `wayland-server`
compositor to a real libwayland client connection through a socket pair. Each
server command is flushed as one batch, so the worker reads it in one batch
too: for example, a press, the capability removal, then the frame. A
barrier advertises a new seat and waits for the worker to bind it, which
proves the worker dispatched everything sent earlier. After a Copy that must
be refused, a blocking load orders the store before later Wayland events and
must be refused for lack of focus. The tests need `libwayland-client` but no
display server.

Run them as a standalone package from the repository root. Seeding the
standalone lockfile from the cagents lockfile makes Cargo prune it to this
package, so the tests run against the production versions of sctk,
wayland-client and wayland-backend:

```sh
cp Cargo.lock vendor/smithay-clipboard/Cargo.lock
cargo test --manifest-path vendor/smithay-clipboard/Cargo.toml
```

Cargo cannot run them with `-p smithay-clipboard` from the parent workspace,
because this package is not a workspace member. Its lockfile and build
artifacts are local test output.

Against the base source, all seven tests fail. The worker panics at
`src/state.rs:395` (pointer), `:320` and `:359` (seat) and `:157` (keyboard),
and the two focus tests' worker answers the refused Copy's load as if the
keyboard were focused. Reverting any one behavior change other than the
`store_selection` guard makes its test fail.

## Upstream

As of 2026-10-21, the panicking unwrap is still present in
Smithay/smithay-clipboard `master` (`26c2f53f15f6bdc4f41a442d0ae2c2d63bbc617c`) and in pop-os
`master` (`eebb02816e919b6fc7456843469105560a53f170`) and `dnd`
(`5a3007def49eb678d1144850c9ee04b80707c56a`). Remove this Cargo patch
when the smithay-clipboard revision used by window_clipboard handles released
input devices. Tracked in
[cagents #1464](https://github.com/CarsonBurke/cagents/issues/1464).
