# Targeted native accessibility button actions

Upstream: https://github.com/pop-os/libcosmic.git
Revision: caec74c2559924443f12fc6faf97a5bcefe6271d
Bundled cosmic-icons submodule: 5252095787cc96e2aed64604158f94e450703455

This copy contains only the libcosmic package, its embedded fonts, translations,
and scalable bundled icons required by build.rs on non-Unix/macOS targets.
Sibling packages and the iced workspace are not vendored here. Their original
path dependencies point to the exact same pinned Git revision. Inherited registry
dependencies are expanded from the upstream workspace with their original version
and default-feature settings. Package features are unchanged.

The button event helper now checks IdEq against its own widget ID before handling
or capturing native accessibility actions. Upstream ignored this ID and captured
all actions, preventing later siblings (including editable workspace fields) from
receiving them and letting an unrelated button react to Click. Native numeric IDs
must match the numeric component of a named widget ID, hence IdEq instead of
ordinary PartialEq.

The application regression mounts unrelated buttons before an editable field,
then dispatches numeric IDs from the emitted accessibility tree. It checks editing
reaches the field and a targeted click reaches only the intended button.

## Runtime interface typography defaults

The text helper exposes a UI-thread default size, set by the application's
accepted interface typography before building a view. Unsized toolkit text,
including context-menu labels, receives that size explicitly instead of keeping
the renderer's startup default. Explicit per-label sizes still take precedence.

Toolkit text buttons use the same default size and grow their default line box
and minimum height accordingly. Menu buttons retain their existing minimum
height and grow when a larger selected font needs more room. These changes do
not write desktop preferences or change package features and dependencies.

Application layout regressions exercise toolkit text, text buttons and an actual
right-click context-menu overlay at small and large interface sizes. The menu
fixture initializes the retained widget tree before dispatching native events.
