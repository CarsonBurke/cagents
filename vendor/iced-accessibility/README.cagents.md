# Iced accessibility ID hashing patch

Source: `iced/accessibility` from [pop-os/libcosmic](https://github.com/pop-os/libcosmic/tree/caec74c2559924443f12fc6faf97a5bcefe6271d/iced/accessibility), revision `caec74c2559924443f12fc6faf97a5bcefe6271d`. The upstream MIT license is included. The manifest retains the pinned dependency graph.

Custom widget IDs compare their names for equality. Their hash must also use the name, rather than the allocation counter, so a rebuilt widget can find its previous entry in a hash map. The same correction exists in `iced-core` for builds without accessibility; accessibility builds reexport this crate's ID type.

Regression tests cover repeated named-ID lookup and replacement, unique IDs, and ID sets containing independently rebuilt named IDs. The native application also tests its selected widget ID type and grouped transcript anchoring across a page transition. Remove this patch when the pinned upstream dependency provides equivalent hashing.
