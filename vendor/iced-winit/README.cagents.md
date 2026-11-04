# Iced native shell backports

Sources copied from libcosmic caec74c2559924443f12fc6faf97a5bcefe6271d,
iced/winit. The standalone manifest preserves that revision's dependencies.

The pinned shell created AccessKit adapters but never published its widget
snapshots. This patch forwards native window events, uses the same root ID for
activation and updates, tracks activation per window, and publishes the current
widget tree and semantic focus after drawing. The root transform maps logical
widget coordinates to native window coordinates. Accessibility focus requests
use the existing widget focus operation. Snapshot work runs only while assistive
technology has activated that window.

The app also uses an accessibility-preserving copy of Iced Responsive in
src/ui_controls/responsive.rs and forwards transcript wrapper nodes. Remove
these backports when the pinned dependency provides equivalent behavior.

The hot-patch callback also wakes the native event loop after queuing its existing
reload action. The callback runs outside the normal proxy worker, whose wake-up
would otherwise be bypassed, leaving an idle window unchanged until another
native event arrives. This change is confined to `debug::on_hotpatch`; ordinary
proxy traffic retains the pinned implementation. The callback is inactive when
Iced's hot feature is disabled. Remove this backport when the pinned dependency
provides the equivalent wake-up.

The pinned window_clipboard revision returns no raw MIME data on X11. The
clipboard bridge therefore uses its existing public X11 Context for bounded
selection reads on actual Xlib/Xcb windows. It negotiates TARGETS in requested
MIME order and supports direct/ICCCM INCR transfers, with a fresh requestor per
read, a 16 KiB TARGETS cap, 32 MiB payload cap, and one three-second selection
owner deadline. Property lengths are checked before fetching bytes; malformed,
changed-owner and stalled transfers are refused. Existing text and Wayland raw
reads keep their original paths. Raw-transfer errors remain logs/None under the
generic clipboard API. Remove this fallback when the pinned backend provides
equivalently bounded raw X11 reads. The ignored raw_selection_fixture test pairs
with a real private X11 owner for direct, INCR and malformed-transfer coverage.

The pinned shell kept an already-elapsed `WaitUntil` deadline whenever a later
one was requested, because it always preferred the earlier deadline. A redraw
scheduled with `RedrawRequest::At` while an older deadline was still the event
loop's control flow therefore left the loop polling with a zero timeout until
the new deadline arrived: one timed animation held the UI thread busy for most
of every interval between its frames. An elapsed deadline is no longer pending
and now yields to the requested one; pending deadlines keep their precedence.
Remove this backport when the pinned dependency merges control flow the same way.

The client-side resize border no longer covers edges that the compositor reports
as tiled. xdg-shell describes a tiled edge as adjoining another part of the
compositor's layout, so a client should not offer resizing there. Without this
check, the border took presses from content at those edges, such as the
scrollbar of a pane at the window's right edge, in tiled niri and COSMIC
windows. Maximized and fullscreen windows still have no border, and floating
windows keep it on every edge. A press is located by its own position and
checked against the current window state, because the pointer can re-enter the
window without moving and the state can change under a still pointer. The border
is scaled by the window's current scale factor on every event, so it follows the
window between outputs. Remove this backport when the pinned dependency honors
tiled edges.
