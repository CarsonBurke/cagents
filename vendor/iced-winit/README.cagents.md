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
