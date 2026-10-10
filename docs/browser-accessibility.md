# Browser accessibility

A browser pane exposes an application-owned **Browser host** group around its toolbar and content. The group identifies its card and sanitized location, reports model focus, browser capability, and loading, ready, failed, suspended, or unavailable state. Its identity remains stable across state updates. When the integrated embedding is unavailable, it includes the closed, typed [Wayland capability reason](browser-wayland.md), including exact missing required protocol names. Toolbar controls retain their own accessibility nodes and keyboard behavior.

Embedded webpages currently have no supported DOM accessibility bridge. The host group does not expose webpage text, forms, or a synthetic document tree. Use **Open in system browser** to access the webpage with your browser's accessibility support. This action is available in integrated and browserless builds, reads the current saved URL when activated, and reports its result with the originating card identity. It preserves the application's focused pane. Opening externally leaves the saved browser descriptor intact.

Public browser locations use `SanitizedLocation`: HTTP(S) locations omit user info, query, and fragment; other schemes omit their payload. Page-controlled titles and backend failure text can contain secrets, so application browser chrome uses a fixed **Browser** title and fixed failure messages. Explicit human pane labels remain visible; page titles and full navigation URLs remain in internal state. URL editing controls follow the private-field policy described by their implementation, without inventing a sanitized editable value with mismatched selection offsets.

## Release smoke test

Run the browserless and integrated development runners sequentially with matched executables. Use private state and a provider-free local page; follow the repository's desktop isolation rules. Scope AT-SPI queries to the exact owned UI PID, and stop all owned fixture processes afterward.

1. Restore a browser descriptor containing user info, query, and fragment. Inspect the rendered accessibility tree: one **Browser host** group, a sanitized location, the correct card identity, and a clear explanation that embedded webpage accessibility is unavailable. Verify every property's text, including text children and selection metadata, omits the sensitive URL parts. Repeat for an opaque URL payload.
2. In an integrated build, observe loading and ready state, then a controlled browser failure or unavailable surface. Confirm the group's numeric AccessKit identity stays stable and controls remain descendants. In browserless mode, expect **System browser only** and **Unavailable** while retaining the saved descriptor.
3. Focus a toolbar control using keyboard navigation. Change load/capability state without user navigation. Verify focus remains on that control and no duplicate host group appears.
4. Activate **Open in system browser** through AT-SPI's button action and with the keyboard. Use a private desktop opener fixture to verify it receives the complete current navigation URL; navigate or change the descriptor before activation to rule out a retained stale URL. Test accepted and refused opener outcomes. Confirm the fixed result identifies the same browser card, the descriptor is retained, and application model focus does not change.
5. Give the page a credential-bearing title. Check its pane header, window title, pane navigation, host group, and failure surface do not echo that page title. Give the pane an explicit human label and confirm that label remains visible.

Source tests accompany this contract. Compilation, tests, and actual AT-SPI/native smoke results must be recorded in the PR before release; this procedure itself is not evidence that they passed.
