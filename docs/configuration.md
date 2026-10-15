# Configuration

- [Files and directories](#files-and-directories)
- [Settings file](#settings-file)
- [Settings keys](#settings-keys)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Terminal themes](#terminal-themes)
- [Language servers](#language-servers)
- [Upgrading older settings files](#upgrading-older-settings-files)
- [Migrating from settings.json](#migrating-from-settingsjson)
- [File safety](#file-safety)
- [Default shell](#default-shell)

Most preferences are edited from **Settings** (the gear in the lower-left
corner). Valid changes save automatically; an invalid draft shows an error
without replacing the last valid value.

## Files and directories

cagents follows the XDG base-directory convention. Production paths are:

- Settings: `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
- Session database: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/sessions.sqlite3`
- Named-window databases: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/windows/<name>/sessions.sqlite3`
- Full-upgrade checkpoint: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/daemon-upgrade/`
- Agent attention journal: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/agent-attention.sqlite3`
- Terminal transcripts: `${XDG_DATA_HOME:-$HOME/.local/share}/cagents/transcripts/`
- Cache, when present: `${XDG_CACHE_HOME:-$HOME/.cache}/cagents/`
- Daemon socket and lock: `$XDG_RUNTIME_DIR/cagents/` (shared by full and
  browserless builds; protocol changes do not create a second owner)

Development builds substitute `cagents-development` for the final `cagents`
directory component. Program files installed by `install.sh` live separately
under `${XDG_DATA_HOME:-$HOME/.local/share}/cagents-install/`.

## Settings file

Preferences live in `${XDG_CONFIG_HOME:-$HOME/.config}/cagents/settings.toml`
(`cagents-development` in place of `cagents` for development builds). The
Settings pane edits the same file, so most people never open it by hand. A few
preferences exist only in the file: language servers and a custom terminal
palette.

The file uses schema version 3. `schema_version` identifies the format; all
other missing keys take their defaults. Unknown keys are
ignored and preserved on save. Values this build cannot read use their default
with a warning and remain in the file until that setting is changed.
Unsupported schema versions and malformed TOML are
errors. A file that fails to load is reported when the
app starts. That session then runs on defaults and does not write the file, so
your file is not overwritten. To replace a broken file from the app, use
**Reset all settings**.

Each window reads the file when it opens. A save re-reads the file and writes
only the fields that window changed since its last save, so several windows can
share the file and edits you make by hand while the app runs are kept. Running
windows do not pick up hand edits; windows opened afterwards do. If a hand edit
leaves the file invalid, saves from running windows fail until it is fixed.

### Example

This example shows common preferences and shortcut defaults. Omitted optional
fields use their defaults. When `language_servers` is missing, the built-in
list applies (see [Language servers](#language-servers)).

```toml
schema_version = 3
max_hot_cards = 6
hibernation_delay_ms = 30000
focus_follows_mouse = false
app_theme = "system"
ui_scale = 1.0
terminal_line_height = 1.286
terminal_scroll_speed = 1.0
terminal_padding = 8.0
terminal_scroll_size = 2
terminal_cursor_style = "beam"
terminal_theme = "auto"
terminal_font_size = 14.0
terminal_font_family = ""
sidebar_width = 260.0
inspector_width = 380.0
default_column_fraction = 0.5
terminal_scrollback_lines = 10000
link_open_destination = "integrated_browser"
link_open_gesture = "control_click"
pull_request_open_target = "review"
managed_worktree_directory = "worktrees/"
worktree_sort = "newest"
editor_auto_save = true
editor_auto_close = true
editor_tab_width = 4
editor_wrap_lines = false
editor_syntax_theme = "auto"
confirm_terminal_close = true
chat_enter_sends = true
show_status_bar = true
relaunch_agents_on_upgrade = true
check_for_updates = true
shortcut_defaults_revision = 4

[keyboard_shortcuts]
new_terminal = ["Ctrl+Shift+T"]
new_native_thread = ["Ctrl+T", "Ctrl+Shift+N"]
chat_account = ["Ctrl+Shift+A"]
chat_model = ["Ctrl+Shift+M"]
chat_permissions = ["Ctrl+Shift+O"]
new_channel = ["Ctrl+Shift+G"]
clone_pane = ["Ctrl+Shift+D"]
close_focused_column = ["Ctrl+W"]
search_projects = ["Ctrl+F"]
previous_column = ["Ctrl+PageUp"]
next_column = ["Ctrl+PageDown"]
cycle_pane_width = ["Alt+R"]
previous_workspace = ["Alt+Shift+Tab"]
next_workspace = ["Alt+Tab"]
review_toggle_viewed = ["Alt+V"]
review_next_file = ["Alt+Down"]
review_previous_file = ["Alt+Up"]
review_next_unviewed = ["Alt+Shift+Down"]
review_next_change = ["Alt+J"]
review_previous_change = ["Alt+K"]

[notifications]
enabled = true
success = true
failure = true
waiting = true
while_focused = false
review_requests = true
```

TOML does not allow a top-level key after a table header. Put every plain
`key = value` line, including `language_servers = []`, before
`[keyboard_shortcuts]` or any other `[table]` or `[[array]]` header.

## Settings keys

Floating-point ranges include both ends. NaN and infinity are rejected.

| Key | Type | Default | Accepted values |
| --- | --- | --- | --- |
| `schema_version` | integer | `3` | Exactly `3`. Required |
| `max_hot_cards` | integer | `6` | `1` through `9223372036854775807`. The number of panes kept live; extra background panes can hibernate |
| `hibernation_delay_ms` | integer | `30000` | `0` through `9223372036854775807` milliseconds before a background pane hibernates |
| `focus_follows_mouse` | boolean | `false` | Activates a pane after the pointer crosses into it and stays for 300 ms. Typing cancels the pending activation |
| `app_theme` | string | `"system"` | `"system"`, `"dark"`, `"light"`, `"catppuccin_mocha"`, `"catppuccin_latte"`, `"dracula"`, `"nord"`, `"tokyo_night"`, `"gruvbox_dark"`, or `"solarized_light"`. Colors the application; terminal and syntax colors are set separately |
| `ui_scale` | float | `1.0` | `0.8` through `1.5`. Multiplies display scaling |
| `terminal_line_height` | float | `1.286` | `1.0` through `2.0`, relative to the font size |
| `terminal_scroll_speed` | float | `1.0` | `0.25` through `4.0`. Multiplies wheel and trackpad scrolling. Mouse wheel events sent to applications that track the mouse are not scaled |
| `terminal_padding` | float | `8.0` | `0.0` through `24.0` logical pixels |
| `terminal_scroll_size` | integer | `2` | `1` through `10` lines per mouse-wheel notch. Trackpad scrolling stays proportional |
| `terminal_cursor_style` | string | `"beam"` | `"application"` uses the cursor shape the program requests. `"block"`, `"beam"`, and `"underline"` override that request |
| `terminal_theme` | string or table | `"auto"` | `"auto"` follows the application's light/dark theme, or choose one of the 25 terminal presets (see [Terminal themes](#terminal-themes)). When **Custom** is selected, the file stores the palette here as a `[terminal_theme.custom]` table |
| `terminal_font_size` | float | `14.0` | `8.0` through `40.0` logical pixels |
| `terminal_font_family` | string | `""` | Empty for the system monospace font, or a family name of up to 128 bytes that is not only whitespace. The terminal grid needs one character width. A family whose glyph widths differ falls back to the system monospace font |
| `editor_font_family` | optional string | absent | Follows the terminal family when absent; otherwise a font family of up to 128 bytes |
| `editor_font_size` | optional float | absent | Follows the terminal size when absent; otherwise `8.0` through `40.0` logical pixels |
| `confirm_multiline_paste` | boolean | `true` | Confirm multiline terminal paste when bracketed paste is unavailable |
| `browser_search_url` | string | `"https://duckduckgo.com/?q={query}"` | HTTP(S) search URL containing `{query}` |
| `sidebar_width` | float | `260.0` | `220.0` through `420.0` |
| `inspector_width` | float | `380.0` | `320.0` through `480.0` |
| `default_column_fraction` | float | `0.5` | `0.25` through `1.0`. The share of the workspace a new pane takes |
| `terminal_scrollback_lines` | integer | `10000` | `0` through `1000000` lines. Applies to terminals started after the change |
| `link_open_destination` | string | `"integrated_browser"` | Where an ordinary link click opens: `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | The gesture that always opens the system browser: `"control_click"` or `"control_shift_click"`. The other gesture always opens the in-app browser |
| `pull_request_open_target` | string | `"review"` | What opening a pull request in the sidebar does: `"review"` opens the in-app review pane, and `"link"` opens the pull request's web page as a link, using the settings above |
| `managed_worktree_directory` | string | `"worktrees/"` | A path that is not blank and has no control characters. It can be absolute or relative to the repository root |
| `worktree_sort` | string | `"newest"` | `"newest"`, `"oldest"`, or `"alphabetical"`. The main worktree always comes first. Imported worktrees with no known creation date come after dated ones, sorted by path |
| `editor_auto_save` | boolean | `true` | Saves edited files after a pause. Auto-save stops when there is a conflict or a save error |
| `editor_auto_close` | boolean | `true` | Pairs brackets and quotes, wraps selected text, and types over an existing closer. Turn it off to insert characters literally |
| `editor_tab_width` | integer | `4` | `1` through `16` columns. Changes native tab stops without rewriting tabs. Also supplies the space indentation fallback when neither file content nor `.editorconfig` sets it |
| `editor_wrap_lines` | boolean | `false` | Wraps long lines in the native editor. When it is off, you scroll sideways |
| `editor_syntax_theme` | string | `"auto"` | Syntax colors for editors and diffs: `"auto"` (follows the app theme), `"solarized_dark"`, `"mocha"`, `"ocean"`, `"eighties"`, or `"inspired_github"` (`"inspired_git_hub"`, written by earlier versions, is also read) |
| `confirm_terminal_close` | boolean | `true` | Asks before closing a terminal that has a foreground program or background jobs. An idle shell closes without asking |
| `chat_enter_sends` | boolean | `true` | When on, Enter sends a chat message and Shift+Enter starts a new line. When off, Enter starts a new line and Ctrl+Enter sends |
| `show_status_bar` | boolean | `true` | Shows the bottom status bar with the usage limits left on each signed-in provider account. Hiding it also stops the usage checks that feed it, unless the Accounts pane is open |
| `relaunch_agents_on_upgrade` | boolean | `true` | When a daemon upgrade finds an agent whose conversation cannot be resumed exactly, relaunches it into its provider's resume picker (or a fresh conversation, when it had none yet) instead of refusing the upgrade. Other running jobs still block the upgrade. `cagents daemon prepare-upgrade` and `install.sh --restart-daemon` follow this unless told otherwise; see [Daemon and upgrades](daemon.md#full-daemon-upgrades) |
| `check_for_updates` | boolean | `true` | Looks for a new release at startup and every six hours. A background check that cannot reach the release server is logged and retried with backoff rather than reported; **Settings → About → Check for updates** always runs and reports its result |
| `language_servers` | array of tables | built-in list | See [Language servers](#language-servers). An empty list turns off language servers |
| `shortcut_defaults_revision` | integer | `4` | The revision of the built-in shortcut defaults the file's `[keyboard_shortcuts]` has been upgraded to. Written by cagents; a file without it is treated as revision `1`. See [Upgrading older settings files](#upgrading-older-settings-files) |
| `keyboard_shortcuts.*` | array of strings | shown above | See [Keyboard shortcuts](#keyboard-shortcuts) |
| `notifications.enabled` | boolean | `true` | Enables desktop agent alerts |
| `notifications.success` | boolean | `true` | Alerts for explicit successful turn completion |
| `notifications.failure` | boolean | `true` | Alerts for failures and cancellations |
| `notifications.waiting` | boolean | `true` | Alerts when a supported provider needs attention |
| `notifications.while_focused` | boolean | `false` | Also alerts for the focused pane in a focused window |
| `notifications.review_requests` | boolean | `true` | Alerts for incoming GitHub review requests |

## Keyboard shortcuts

Each action takes a list of chords. A chord has one key and one or more
modifiers, joined by `+`. Names are case-insensitive.

- Modifiers: `Ctrl` (or `Control`), `Alt`, `Super` (or `Meta`), and `Shift`.
  Each chord needs `Ctrl`, `Alt`, or `Super`, except function keys, which can
  stand alone. `Shift` is optional.
- Keys: `A`-`Z`, `0`-`9`, `Left`, `Right`, `Up`, `Down`, `Tab`, `PageUp`, or
  `PageDown`, punctuation, or `F1`–`F12`. Use `Comma` and `Plus` for separator
  keys, for example `Ctrl+Comma`.
- No chord can appear twice, even under different actions.
- A chord you bind here is not passed to the program running in a pane.
  For that reason, bare `Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+S`, and `Ctrl+Q`
  cannot be bound. Versions with more modifiers, such as `Ctrl+Shift+C`, can
  be bound. Other keys a shell uses, such as `Ctrl+W`, `Ctrl+T`, and `Alt`
  with a letter or arrow, can be bound but then no longer reach a terminal.
  The defaults bind **Ctrl+W** to close the focused pane, **Alt+R** to cycle
  pane width, and **Ctrl+T** to create a native thread tab.
- The `search_projects` chords reach terminal programs, except **Ctrl+F**,
  which opens terminal scrollback find. The same key finds in the focused
  editor or chat transcript, and otherwise searches projects.
  File and hunk navigation also works in local diff panes. Viewed-file commands
  act only in a pull request pane. The chords reach terminals outside that scope.
- `chat_account`, `chat_model`, and `chat_permissions` apply only to the
  focused native chat. Account selection is available before the conversation
  starts. Up/Down or Tab/Shift+Tab choose an account or permission mode, and
  Enter confirms it. The model picker accepts a search followed by Enter.
- `new_terminal`, `close_focused_column`, `search_projects`,
  `previous_column`, and `next_column` need at least one chord. The rest
  including the command palette, pane actions and the six `review_*` actions
  can be empty lists (`[]`), which leaves
  them unbound.

## Terminal themes

There are 25 dark and light terminal presets. The values are `"dark"`,
`"light"`, `"solarized_dark"`, `"solarized_light"`, `"dracula"`,
`"catppuccin_mocha"`, `"catppuccin_macchiato"`, `"catppuccin_frappe"`,
`"catppuccin_latte"`, `"nord"`, `"gruvbox_dark"`, `"gruvbox_light"`,
`"tokyo_night"`, `"tokyo_night_storm"`, `"tokyo_night_day"`, `"rose_pine"`,
`"rose_pine_moon"`, `"rose_pine_dawn"`, `"kanagawa_wave"`, `"kanagawa_lotus"`,
`"one_dark"`, `"github_dark"`, `"github_light"`, `"everforest_dark"`, and
`"everforest_light"`. Choose one under **Settings → Terminal → Color theme**.

To make your own palette:

1. Add a `[custom_terminal_theme]` table after the top-level keys.
2. Restart the app.
3. Choose **Custom** in the terminal color theme dropdown.

cagents keeps this table when you switch to a preset. On load, this table
replaces any palette stored under `terminal_theme`, so editing the table
updates an active custom theme.

The table rules:

- Colors use `#RRGGBB`.
- `ansi` has exactly 16 colors: normal colors 0-7, then bright colors 8-15.
- `light` is optional and defaults to `false`. It tells terminal programs
  whether the palette is light or dark.
- Colors that programs set themselves, with truecolor or OSC palette changes,
  take precedence over this palette.

```toml
[custom_terminal_theme]
foreground = "#d8d8d8"
background = "#101010"
cursor = "#ffffff"
light = false
ansi = [
  "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
  "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
  "#4c566a", "#d08770", "#b1d196", "#f0d399",
  "#8fbcdb", "#c5a0c0", "#93ccdc", "#eceff4",
]
```

## Editor navigation

Ctrl+G accepts a one-based line or `line:column`. The Ln/Col indicator opens the
same field and shows the selected character count. File filtering also accepts
compiler locations such as `src/main.rs:120:8`.

Alt+Left and Alt+Right, or the mouse back and forward buttons over the focused
editor, revisit explicit jumps such as definitions, search results and go-to-line.
Switching between saved files retains undo history for up to eight files and
32 MiB of edit data per card. Reopening a file whose disk content changed drops
its retained history. Unsaved edits still require saving or discarding before
switching files.

Tab and Shift+Tab indent or outdent multiline selections as one undo step. Enter keeps
leading whitespace and adds a level after an opening bracket. The editor detects
the file's indentation and applies matching `.editorconfig` settings from within
the repository; the tab-width preference supplies the fallback.

## Language servers

`language_servers` lists external Language Server Protocol (LSP) servers for
the native editor. If the key is missing, cagents uses this built-in list. If
the key is present, its list replaces the built-in one entirely; the two are
not merged. `language_servers = []` turns off language servers.

| id | Command | Extensions |
| --- | --- | --- |
| `rust-analyzer` | `rust-analyzer` | `rs` |
| `pyright` | `pyright-langserver --stdio` | `py` |
| `typescript-language-server` | `typescript-language-server --stdio` | `js`, `mjs`, `cjs`, `jsx`, `ts`, `mts`, `cts`, `tsx` |
| `gopls` | `gopls` | `go` |
| `clangd` | `clangd` | `c`, `h`, `cc`, `cpp`, `cxx`, `hh`, `hpp`, `hxx` |
| `lua-language-server` | `lua-language-server` | `lua` |
| `bash-language-server` | `bash-language-server start` | `sh`, `bash` |

A server entry has these fields:

- `id`: a unique name that is not empty.
- `command`: an argument list, not a shell command line. Its first item names
  the executable. A plain name is looked up on `PATH`. A name that contains `/`
  is treated as a path.
- `languages`: one or more tables. Each has an LSP `language_id` and at least
  one `extensions` value. Write extensions in lowercase without the leading
  dot. When cagents matches a file, the case of the file's extension does not
  matter.
- `root_markers` (optional): relative paths that mark a project root, such as
  `Cargo.toml`. They cannot contain empty, `.`, or `..` path components. To
  find a file's project root, cagents checks the file's folder and then each
  parent folder, and stops at the worktree root. The first folder that contains
  a marker is the project root. If no folder has a marker, the worktree root is
  used.

An extension can belong to only one server.

```toml
[[language_servers]]
id = "rust-analyzer"
command = ["rust-analyzer"]
root_markers = ["Cargo.toml", "rust-project.json", ".git"]

[[language_servers.languages]]
language_id = "rust"
extensions = ["rs"]
```

`[[language_servers]]` headers can appear before or after
`[keyboard_shortcuts]`. The inline `language_servers = []` must come before
every table header.

## Upgrading older settings files

cagents upgrades version 1 and version 2 files when it loads them. It writes
them back as version 3 the next time it saves.

- **Version 1:** a `sidebar_width` from 180 through 480 is clamped to the
  current range of 220 through 420. Other values are kept as written.
- **Version 2:** these files get the default `inspector_width` and the built-in
  language servers. A shortcut written as a single string becomes a list with
  one chord.
- **Old scroll key:** early version 3 files set lines per notch with an integer
  `terminal_scroll_speed`. If `terminal_scroll_size` is missing, that value is
  moved to `terminal_scroll_size`, and an old value of `1` becomes the new
  default of `2`.
- **Renamed shortcut:** a `new_thread` shortcut, the name used before channels
  were named, is read as `new_channel`. If both names are present,
  `new_channel` is used.
- **New shortcut defaults:** a file without `shortcut_defaults_revision` was
  written against the first defaults. Each action still bound to exactly its
  old default, in any order or spelling, moves to the current one:

  | Action | Old default | Current default |
  | --- | --- | --- |
  | `new_terminal` | `Ctrl+T` | `Ctrl+Shift+T` |
  | `new_channel` | `Alt+G` | `Ctrl+Shift+G` |
  | `clone_pane` | `Alt+E` | `Ctrl+Shift+D` |
  | `close_focused_column` | `Ctrl+Shift+W` (revisions 2–3) | `Ctrl+W` |
  | `previous_column` | `Alt+Q`, `Alt+Left` | `Ctrl+PageUp` |
  | `next_column` | `Alt+W`, `Alt+Right` | `Ctrl+PageDown` |
  | `cycle_pane_width` | `Ctrl+Shift+R` (revisions 2–3) | `Alt+R` |
  | `new_native_thread` | `Ctrl+Shift+N` | `Ctrl+T`, `Ctrl+Shift+N` |

  An action the table leaves out counts as on its old default. A version-2
  file that spells `previous_column` as `"Alt+Q"` or `next_column` as
  `"Alt+W"`, the defaults before chord lists, moves too. An action you
  changed, extended, or emptied keeps your chords. A new chord
  another action already uses is not taken from it; if that leaves an action
  with no new chord, it keeps its old default. The next save records
  `shortcut_defaults_revision = 4`, after which an old default you choose
  again is kept.
- **Newer shortcuts:** a `[keyboard_shortcuts]` table that lacks a newer action
  gets its default chord. These actions are `new_channel` (Ctrl+Shift+G),
  `clone_pane` (Ctrl+Shift+D), `cycle_pane_width` (Alt+R),
  `previous_workspace` (Alt+Shift+Tab), `next_workspace` (Alt+Tab),
  `review_toggle_viewed` (Alt+V), `review_next_file` (Alt+Down),
  `review_previous_file` (Alt+Up), `review_next_unviewed` (Alt+Shift+Down),
  `review_next_change` (Alt+J), and `review_previous_change` (Alt+K). If
  another action already uses that chord, the existing binding is kept and the
  new action stays unbound (`[]`) until you give it a free chord.
- **Your bindings:** any binding you set yourself is kept, including an empty
  list. A conflicting binding that you set yourself is still an error.

In the Settings pane, text fields save valid edits after a short pause.
Leaving or closing the pane also applies valid pending edits. Invalid fields
and disk-write failures stay on screen as errors. Your existing preferences,
such as an `"application"` cursor, are kept and are not replaced by newer
defaults.

## Migrating from settings.json

cagents migrates a `settings.json` in the same directory only when
`settings.toml` does not exist. The JSON file must use schema version 1. Every
field is optional, and unknown fields are rejected. cagents checks the JSON,
writes and syncs the TOML file, and then tries to delete the JSON file. If
deletion fails, the JSON file stays and is ignored. An existing TOML file
always takes precedence. If the TOML file is malformed, cagents reports the
error and does not fall back to the JSON file.

## File safety

- cagents sets the configuration directory to mode `0700` and the settings file
  to mode `0600`.
- Both must be owned by the effective user.
- If any component of the path is a symbolic link, cagents rejects the path.
- Saves write a temporary file and rename it into place.
- A `settings.toml.lock` file keeps separate windows from saving at the same
  time.

## Default shell

New terminal panes use a default shell. So does `cagents terminal spawn` when
you give it no command. cagents tries these in order and uses the first one
that is an absolute path to an executable file:

1. The shell in the effective user's passwd entry.
2. `SHELL`.
3. `/bin/sh`.

This lookup applies only when cagents creates a new default terminal.
Explicit CLI commands and launch recipes restored from saved sessions run
unchanged.
