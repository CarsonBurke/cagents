# Terminal accessibility

Each mounted terminal card exposes a stable AccessKit terminal node named from
its card title and identity. Its read-only text interface contains the current
visible grid only, capped to 128 rows, 8192 cells, and 16 KiB of UTF-8 text.
Concealed cells remain concealed; hyperlinks, transcripts, hidden scrollback,
arguments, and working directories are not part of the semantic tree. Very long
Unicode clusters that cannot fit AccessKit's per-character byte length are
omitted, and the terminal description announces a capped view.

Choose **Review visible text** in the terminal context menu or command palette
to navigate without typing into the child process. The palette provides keyboard
entry through the existing command-palette shortcut. A virtual review action is
also exposed to assistive technology, without a permanent bar, icon or pointer
hotspot in normal terminal input. Assistive focus enters review, and both the
outgoing and incoming terminal suppress PTY focus-report escape sequences during
that handoff. Return to input restores ordinary focus reports.

Only active review shows a status row saying typing is blocked, with **Return to
input**; Escape also exits. Arrow keys move an assistive caret; Shift extends its
selection. Other typing, chords, IME, paste and mouse input are consumed locally.
The release of Escape and other consumed keys is withheld from the child. The
app retains that barrier across card focus changes and unmounting, including
repeats of a key still held when review ends. Outside review, ordinary terminal
input retains its existing behavior.

Cursor and selection facts belong to the text interface and description, rather
than being appended to an unbounded accessible name. Assistive selections must
address current projected text nodes and valid character offsets. They cannot
select or scroll to hidden history, send keys, paste, or activate terminal links.
Rapid output refreshes semantic text at most every 100 ms, with a scheduled final
refresh even when output stops. Mount, focus, resize, and alternate-screen changes
refresh immediately. Normal input uses the full widget grid. Active review's
24-pixel status row changes the actual viewport through ordinary terminal
resizing, so the child can reflow and keep its latest output visible. This is a
viewport resize, not PTY typing. Visible text coordinates are reprojected and old
assistive selections are invalidated at both the mode transition and the fresh
matching terminal frame; they cannot silently select reflowed text.

## Release smoke

Unit assertions do not prove screen-reader behavior. After building a matched
browserless development set, open a deterministic private fixture terminal and
record its owned UI PID. Keep its XDG roots and nested compositor separate from
ordinary application state. On that session's accessibility bus run:

```sh
scripts/probe-terminal-accessibility --pid OWNED_UI_PID
scripts/probe-terminal-accessibility --pid OWNED_UI_PID --toggle-review
scripts/probe-terminal-accessibility --pid OWNED_UI_PID --select-first-character
```

The probe requires the current user's explicit PID and visits only that app's
subtree. It never sends keyboard input. Confirm the terminal role, card name,
focus, text interface, capped content, and cursor/selection values. Then use Orca
on the owned fixture: navigate visible text, enter and exit review, try arrows,
chords, and paste while the fixture records child input, and confirm no bytes
arrive during review. Check ordinary typing afterward, resize, rapid output with
a final quiet frame, card migration, and unmount. Stop the owned fixture and its
private daemon/compositor. If AT-SPI registration or Orca is unavailable, record
that limitation; do not claim a completed native screen-reader acceptance.
