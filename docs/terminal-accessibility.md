# Terminal accessibility

Each mounted terminal card exposes a stable AccessKit terminal node named from
its card title and identity. Its read-only text interface contains the current
visible grid only, capped to 128 rows, 8192 cells, and 16 KiB of UTF-8 text.
Concealed cells remain concealed; hyperlinks, transcripts, hidden scrollback,
arguments, and working directories are not part of the semantic tree. Very long
Unicode clusters that cannot fit AccessKit's per-character byte length are
omitted, and the terminal description announces a capped view.

Use **Review visible text** in the terminal's small control bar to navigate
without typing into the child process. The same button is exposed to assistive
technology. Assistive focus enters review, and both the outgoing and incoming
terminal suppress PTY focus-report escape sequences during that handoff. Return
to input restores ordinary focus reports. In review mode arrow keys move an assistive caret; Shift extends its
selection. Other typing, chords, IME, paste, and mouse input are consumed locally.
The bar says that typing is blocked. **Return to input** or Escape exits review;
the release of Escape and other consumed keys is also withheld from the child.
The app retains that barrier across card focus changes and unmounting, including
repeats of a key still held when review ends.
Outside review, ordinary terminal input retains its existing behavior.

Cursor and selection facts belong to the text interface and description, rather
than being appended to an unbounded accessible name. Assistive selections must
address current projected text nodes and valid character offsets. They cannot
select or scroll to hidden history, send keys, paste, or activate terminal links.
Rapid output refreshes semantic text at most every 100 ms, with a scheduled final
refresh even when output stops. Mount, focus, resize, and alternate-screen changes
refresh immediately. The control bar reserves 24 pixels above the grid, so the
visible text and pointer coordinates refer to unobscured terminal cells.

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
