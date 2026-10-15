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
ignored and preserved on save. At startup, values this build cannot read use their default
with a warning and remain in the file until that setting is changed.
Unsupported schema versions and malformed TOML are
errors. A file that fails to load is reported when the
app starts. That session then runs on defaults and does not write the file, so
your file is not overwritten. To replace a broken file from the app, use
**Reset all settings**.

Use **Open settings.toml** or **Reveal settings.toml** in the command palette
or **Settings > General**. Opening creates a file with the current values if
one does not exist, including commented defaults for reference. Missing values
always use the documented defaults.

Every window watches the settings directory, including atomic file replacements.
After a short quiet interval, valid hand edits and saves from other windows apply
without restarting. Invalid TOML or invalid known values leave the last working
settings active and show a persistent notice with **Open file** and error details.
Unknown keys remain preserved. A save re-reads the file and merges only the fields
that window changed, preserving other windows' preferences.

**Reset all settings** and **Reset category** first write a private, timestamped
`settings.toml.bak-<timestamp>-<suffix>` alongside the file. **Undo reset** restores
the original bytes, including comments and hand-written tables. Backups remain on
disk for recovery after closing the window. Conversations and panes are preserved.

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
interface_font_family = ""
interface_font_size = 13.0
chat_font_family = ""
chat_font_size = 14.0
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
terminal_scrollback_mib = 16
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
shortcut_defaults_revision = 7

[keyboard_shortcuts]
new_terminal = ["Ctrl+Shift+T"]
new_native_thread = ["Ctrl+T", "Ctrl+Shift+N"]
chat_account = ["Ctrl+Shift+A"]
chat_model = ["Ctrl+Shift+M"]
chat_permissions = ["Ctrl+Shift+O"]
new_channel = ["Ctrl+Shift+G"]
clone_pane = ["Ctrl+Shift+D"]
close_focused_column = ["Ctrl+Shift+W"]
search_projects = ["Ctrl+Shift+F"]
find_in_pane = ["Ctrl+F"]
previous_column = ["Ctrl+PageUp"]
next_column = ["Ctrl+PageDown"]
cycle_pane_width = ["Alt+R"]
previous_workspace = ["Alt+Shift+Tab"]
next_workspace = ["Alt+Tab"]
review_toggle_viewed = ["Alt+V"]
review_next_file = ["Alt+Down"]
review_previous_file = ["Alt+Up"]
review_next_unviewed = ["Alt+Shift+Down"]
review_next_change = ["Alt+J", "F7"]
review_previous_change = ["Alt+K", "Shift+F7"]

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
| `interface_font_family` | string | `""` | Empty follows the desktop interface family; otherwise a family name up to 128 bytes. Settings lists installed families and warns when a saved family is unavailable |
| `interface_font_size` | float | `13.0` | `10.0` through `24.0` logical pixels for workbench labels and controls |
| `chat_font_family` | string | `""` | Empty follows the interface family; otherwise a family name up to 128 bytes. Applies to transcript prose and Markdown; code stays monospace |
| `chat_font_size` | float | `14.0` | `8.0` through `40.0` logical pixels. Applies to existing transcript messages and the composer |
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
| `terminal_scrollback_mib` | integer | `16` | Per-terminal history page budget, `0` through `1024` MiB. Allocated as output arrives; retained row count depends on width and content. Applies to newly started terminals. Previous `terminal_scrollback_lines` values migrate at 1 KiB per line, rounded up to MiB |
| `link_open_destination` | string | `"integrated_browser"` | Where an ordinary link click opens: `"integrated_browser"` or `"system_default"` |
| `link_open_gesture` | string | `"control_click"` | The gesture that always opens the system browser: `"control_click"` or `"control_shift_click"`. The other gesture always opens the in-app browser |
| `pull_request_open_target` | string | `"review"` | What opening a pull request in the sidebar does: `"review"` opens the in-app review pane, and `"link"` opens the pull request's web page as a link, using the settings above |
| `managed_worktree_directory` | string | `"worktrees/"` | A path that is not blank and has no control characters. It can be absolute or relative to the repository root |
| `worktree_sort` | string | `"newest"` | `"newest"`, `"oldest"`, or `"alphabetical"`. The main worktree always comes first. Imported worktrees with no known creation date come after dated ones, sorted by path |
| `git_status_limit` | integer | `10000` | Maximum changed paths shown in each source-control snapshot; normalized to `1`–`100000`. Oversized listings show a notice. Safety checks and bulk group actions read complete status independently. |
| `editor_auto_save` | boolean | `true` | Saves edited files after a pause. Auto-save stops when there is a conflict or a save error |
| `editor_auto_close` | boolean | `true` | Pairs brackets and quotes, wraps selected text, and types over an existing closer. Turn it off to insert characters literally |
| `editor_tab_width` | integer | `4` | `1` through `16` columns. Changes native tab stops without rewriting tabs. Also supplies the space indentation fallback when neither file content nor `.editorconfig` sets it |
| `editor_wrap_lines` | boolean | `false` | Wraps long lines in the native editor. When it is off, you scroll sideways |
| `editor_find_options` | table | all options `false` | Persisted Find matching options; see the three fields below |
| `editor_find_options.case_sensitive` | boolean | `false` | Match case in the native editor Find strip |
| `editor_find_options.regex` | boolean | `false` | Interpret Find queries as bounded regular expressions |
| `editor_find_options.whole_word` | boolean | `false` | Require word boundaries outside each Find match |
| `editor_replace_preserve_case` | boolean | `false` | Preserve upper, lower and title case in plain-text replacements |
| `editor_syntax_theme` | string | `"auto"` | Syntax colors for editors and diffs: `"auto"` (follows the app theme), `"solarized_dark"`, `"mocha"`, `"ocean"`, `"eighties"`, or `"inspired_github"` (`"inspired_git_hub"`, written by earlier versions, is also read) |
| `diff_ignore_whitespace` | boolean | `false` | Ignores horizontal whitespace when comparing lines, while retaining exact original text for copying and patches. The diff toolbar changes this preference |
| `confirm_terminal_close` | boolean | `true` | Asks before closing a terminal that has a foreground program or background jobs. An idle shell closes without asking |
| `chat_enter_sends` | boolean | `true` | When on, Enter sends a chat message and Shift+Enter starts a new line. When off, Enter starts a new line and Ctrl+Enter sends |
| `show_status_bar` | boolean | `true` | Shows the bottom status bar with the usage limits left on each signed-in provider account. Hiding it also stops the usage checks that feed it, unless the Accounts pane is open |
| `relaunch_agents_on_upgrade` | boolean | `true` | When a daemon upgrade finds an agent whose conversation cannot be resumed exactly, relaunches it into its provider's resume picker (or a fresh conversation, when it had none yet) instead of refusing the upgrade. Other running jobs still block the upgrade. `cagents daemon prepare-upgrade` and `install.sh --restart-daemon` follow this unless told otherwise; see [Daemon and upgrades](daemon.md#full-daemon-upgrades) |
| `check_for_updates` | boolean | `true` | Looks for a new release at startup and every six hours. A background check that cannot reach the release server is logged and retried with backoff rather than reported; **Settings → About → Check for updates** always runs and reports its result |
| `lsp_idle_timeout_seconds` | integer | `600` | Idle language-server lifetime after its final document closes (1 second to 7 days) |
| `lsp_max_sessions` | integer | `8` | Global language-server limit (1–64); idle servers are evicted in least recently used order, active documents are preserved |
| `language_servers` | array of tables | built-in list | See [Language servers](#language-servers). An empty list turns off language servers |
| `palette_recents` | array of strings | `[]` | Recent command and navigation IDs, most recent first. Written by the palette and bounded to 24 entries |
| `shortcut_defaults_revision` | integer | `7` | The revision of the built-in shortcut defaults the file's `[keyboard_shortcuts]` has been upgraded to. Written by cagents; a file without it is treated as revision `1`. See [Upgrading older settings files](#upgrading-older-settings-files) |
| `keyboard_shortcuts.*` | array of strings | shown above | See [Keyboard shortcuts](#keyboard-shortcuts) |
| `notifications.enabled` | boolean | `true` | Enables desktop agent alerts |
| `notifications.success` | boolean | `true` | Alerts for explicit successful turn completion |
| `notifications.failure` | boolean | `true` | Alerts for failures and cancellations |
| `notifications.waiting` | boolean | `true` | Alerts when a supported provider needs attention |
| `notifications.while_focused` | boolean | `false` | Also alerts for the focused pane in a focused window |
| `notifications.review_requests` | boolean | `true` | Alerts for incoming GitHub review requests |
| `custom_terminal_theme` | table | unset | Custom terminal color palette, selectable live as Custom. |
| `auto_delete_merged_worktrees` | boolean | `false` | Clean up clean, inactive merged worktrees automatically; never discard changes or stop running work. |
| `default_worktree_pane` | string | `"native_chat"` | `"native_chat"` or `"terminal"`; the first pane in an empty worktree. |
| `default_channel_pane` | string | `"native_chat"` | `"native_chat"` or `"terminal"`; the first pane in an empty channel. |
| `diff_hide_unchanged` | boolean | `true` | Hide unchanged lines in diff views. |
| `chat_expand_tool_groups` | boolean | `false` | Start chat tool groups expanded. |
| `chat_content_fraction` | float | `0.5` | Legacy preference retained for compatibility; chat uses an automatic centered reading column. |
| `default_native_pane_fraction` | float | `0.5` | Initial width fraction of native chat and utility panes. |
| `usage_preview_rows` | table | `{}` | Provider keys map to usage rows; an empty list hides that provider’s preview rows. |
| `show_worktree_changes_icons` | boolean | `true` | Show dirty-worktree badges in Projects. |
| `keyboard_shortcuts` | table | built-in chords | Action keys map to chord arrays. See Keyboard shortcuts. |
| `notifications` | table | enabled | Notification event preferences. See the example above. |

## Keyboard shortcuts

The command palette, Keyboard settings, and pane context menus share the same
command definitions. Additional commands are unbound by default: `new_browser`,
`open_folder`, `new_worktree`, `close_other_panes`, `close_panes_to_right`,
`copy_path`, `reveal_in_projects`, `fork_conversation`, `focus_sidebar_tree`,
`restart_daemon`, `toggle_theme`, `resume_conversation`, `switch_pane`, and
`review_conversation_changes`. Set their chord lists under `[keyboard_shortcuts]`,
or bind them in **Settings → Keyboard**. For example:

```toml
copy_path = ["Ctrl+Alt+Shift+K"]
close_other_panes = []
```

`focus_project_1` through `focus_project_9` select projects in sidebar order.
The `settings_general`, `settings_appearance`, `settings_terminal`,
`settings_links`, `settings_keyboard`, `settings_native_chat`, `settings_editor`,
`settings_accounts`, `settings_notifications`, and `settings_about` commands
open the corresponding Settings section. These commands also start unbound.
`end_conversation` ends a native conversation through its existing confirmation
flow and starts unbound. Closing a pane tab can instead hide its conversation.

Native code-buffer actions are separately configurable as
`keyboard_shortcuts.editor_undo` (`Ctrl+Z`), `editor_redo` (`Ctrl+Shift+Z`,
`Ctrl+Y`), `editor_cut` (`Ctrl+X`), `editor_copy` (`Ctrl+C`), `editor_paste`
(`Ctrl+V`) and `editor_select_all` (`Ctrl+A`). These bindings apply only to the
editable code buffer. Other text fields and terminal programs retain their own
editing keys. Rebinding or clearing an editor action also replaces its original
code-buffer chord; menu hints follow the current binding.

`next_diagnostic` (`F8`) and `previous_diagnostic` (`Shift+F8`) navigate the
editable buffer's diagnostics. They are available when the buffer has
diagnostics, and their bindings can be changed or cleared in Keyboard settings.

Diff selections use `diff_copy` (`Ctrl+C`) and `diff_select_all` (`Ctrl+A`).
`diff_copy_old`, `diff_copy_new`, `diff_copy_patch`,
`diff_copy_line_reference` and `diff_open_in_editor` start unbound. These
commands act in a focused diff; typing in its Find field retains normal text
editing. A row menu retains its original source and row even if focus moves.

Sidebar commands such as `copy_branch`, `new_worktree_from_branch`,
`rename_channel`, `mark_channel_read`, `mark_worktree_read`, `delete_channel`,
`delete_worktree`, `remove_project` and `delete_selected_rows` start unbound.
Open the palette while the target sidebar row or file has focus to invoke a
command for that target. A stale selection does not retarget the command to a
pane. The `open_in_external_editor` command uses the first configured editor;
a menu entry for another configured editor keeps that editor as its target.


Each action takes a list of chords. A chord has one key and one or more
modifiers, joined by `+`. Names are case-insensitive.

- Modifiers: `Ctrl` (or `Control`), `Alt`, `Super` (or `Meta`), and `Shift`.
  Each chord needs `Ctrl`, `Alt`, or `Super`, except function keys, which can
  stand alone. `Shift` is optional.
- Keys: `A`-`Z`, `0`-`9`, `Left`, `Right`, `Up`, `Down`, `Tab`, `PageUp`, or
  `PageDown`, punctuation, or `F1`–`F12`. Use `Comma` and `Plus` for separator
  keys, for example `Ctrl+Comma`.
- Actions that can run in the same context cannot share a chord. Native editor
  editing actions can share an editing chord with an action that only runs
  outside text controls; Redo and Reopen pane share `Ctrl+Shift+Z` this way.
- Terminal interception follows the action's **Send to terminal** policy in
  Keyboard settings. `terminal_passthrough_actions` stores serialized action
  names whose chords reach the focused terminal. Its defaults include
  `rename_pane`, `new_native_thread`, `find_in_pane`, `cycle_pane_width` and
  `focus_pane_1` through `focus_pane_9`; removing a name explicitly enables
  that action in terminals. Bare `Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+S`, and
  `Ctrl+Q` remain reserved to preserve interruption, EOF and flow control.
- **Ctrl+F** finds in the focused editor or chat, and falls back to project
  search for panes without find. It stays in sidebar and inspector text fields
  when those have keyboard focus. **Ctrl+Shift+F** searches projects outside
  terminals and searches terminal scrollback inside them. Sending
  `search_projects` to the terminal releases this chord too.
- Native text controls retain undo, redo, selection and word navigation even
  when a global action is bound to those editing chords. Reopen pane's
  **Ctrl+Shift+Z** default applies outside text controls.
- File and hunk navigation work in review panes; viewed-file commands apply
  to pull request panes. Their chords reach terminals outside that scope.
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
2. Save the file. Every open window reloads the palette automatically.
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

The settings pane presents width fractions as percentages and hibernation delays
in seconds. The file keeps the documented fractional widths and millisecond
duration so existing configuration continues to round-trip unchanged. Font
selectors accept a typed family as well as an installed choice. An unavailable
family remains saved, with a visible fallback warning.

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

Language servers are shared by workspace root. Rust selects the outermost Cargo
workspace (or nearest Cargo package); JavaScript and TypeScript recognize npm
workspaces, `pnpm-workspace.yaml`, and TypeScript project references. Discovery
runs off the UI thread. The inspector lists live servers and their cached process
RSS, refreshed in the background; RSS describes the server process, excluding its
children. Closing the last document arms the idle timeout. Worktree removal
retires its servers immediately; removals outside the app are detected within
five seconds. When all slots have active documents, opening a new server reports
the limit instead of evicting a server in use. Set the two scalar options before
any TOML table headers.

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
  | `close_focused_column` | `Ctrl+W` (revisions 1, 4–5) | `Ctrl+Shift+W` |
  | `previous_column` | `Alt+Q`, `Alt+Left` | `Ctrl+PageUp` |
  | `next_column` | `Alt+W`, `Alt+Right` | `Ctrl+PageDown` |
  | `cycle_pane_width` | `Ctrl+Shift+R` (revisions 2–3) | `Alt+R` |
  | `new_native_thread` | `Ctrl+Shift+N` | `Ctrl+T`, `Ctrl+Shift+N` |
  | `search_projects` | `Ctrl+F` (revisions 1–4) | `Ctrl+Shift+F` |
  | `find_in_pane` | absent before revision 5 | `Ctrl+F` |

  An action the table leaves out counts as on its old default. A version-2
  file that spells `previous_column` as `"Alt+Q"` or `next_column` as
  `"Alt+W"`, the defaults before chord lists, moves too. An action you
  changed, extended, or emptied keeps your chords. A new chord
  another action already uses is not taken from it; if that leaves an action
  with no new chord, it keeps its old default. The next save records
  `shortcut_defaults_revision = 7`, after which an old default you choose
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

### Language-server editor options

Each `[[language_servers]]` entry accepts `format_on_save` (default `false`),
`formatting_timeout_ms` (default `1000`, range `50..=10000`),
`initialization_options`, and `configuration`. Initialization options are sent
unchanged in `initialize`; configuration is sent after initialization and used
for `workspace/configuration` section requests. Dotted sections such as
`rust-analyzer.check` select nested configuration objects. Each JSON value is
limited to 1 MiB. For example, these fields belong to the rust-analyzer server:

```toml
format_on_save = true
formatting_timeout_ms = 1000
initialization_options = { check = { command = "clippy" } }
configuration = { rust-analyzer = { check = { command = "clippy" } } }
```

Formatting timeouts report a terminal request failure and cancel the server
request, allowing the application to continue saving. Workspace-edit file
creation, file deletion, and file renaming are rejected before applying any
text edits; ordinary multi-file text edits retain server versions and preview
annotations for the application to validate and confirm.

### Language-server editor commands

With the text editor focused, `F2` prepares a symbol rename and opens an inline
name field. Submit the field to inspect the proposed changes in a per-file diff
preview, then apply or cancel. `Ctrl+.` requests code actions at the caret or
selection. Disabled actions retain the server's explanation. The command palette
also exposes Rename symbol, Format document, Format selection, Code actions, and
Show signature help when the current server supports them. Signature help follows
the server's trigger characters, emphasizes the active parameter, and closes on
`)` or Escape.

Rename and multi-file text edits require preview confirmation. Each participating
open buffer retains one independent Undo step and remains unsaved. Unopened files
use atomic, conflict-aware writes; if a later file refuses its write, earlier
writes are restored only while their written contents still match. A concurrent
outside edit is preserved and reported. Changed buffers, changed tab identities,
overlapping ranges, unsupported file operations, and paths outside the worktree
refuse the operation before application. Previews support up to 512 files and a
64 MiB combined before/after text budget; ordinary editable-file limits still
apply. Server confirmation labels and descriptions appear above the comparison.

With `format_on_save = true`, explicit saves and autosaves request formatting
first. A timeout or refusal reports the failure and continues the ordinary safe
save; formatting never bypasses disk conflict checks. Formatting edits also
retain Undo.

Definitions outside the project open a highlighted, read-only source tab. Such
tabs retain their original project's language-server workspace, so definition
navigation continues to work inside dependency sources. Their source path and
caret restore locally with the window; they do not become shared workspace
recipes. Typing, clipboard cuts/pastes, formatting, refactoring, history replay,
and saving cannot modify these dependency sources.

Server errors remain dismissible notices. Informational server messages expire
automatically, and server log messages go to the diagnostic log.

The command registry also exposes `keyboard_shortcuts.mark_all_read` and
`keyboard_shortcuts.clone_repository` (both unbound by default), and
`keyboard_shortcuts.search_in_project` (`Ctrl+Alt+F`). `focus_last_pane`
uses `Alt+0`; `next_attention` and `previous_attention` follow the configured
attention navigation bindings. All of these appear in Keyboard settings.

The unbound Git command settings are `git_fetch`, `git_push`, `git_publish`,
`git_sync`, `git_sync_rebase`, `git_stash`, `git_stash_untracked`, `git_stashes`,
`git_apply_stash`, `git_pop_stash`, `git_drop_stash`, `git_commit`, `git_amend`,
`git_refresh`, `git_abort`, `git_continue` and `git_output`, under
`[keyboard_shortcuts]`. They use the active worktree's existing Git controls;
commit uses that worktree's subject, description and amend choice. Stash
commands open a target picker and retain its exact stash object identity.

`git_accept_ours` and `git_accept_theirs` are unbound commands for the focused
conflict row. They accept the chosen side and stage that exact path, using the
same operation as the row buttons; a changed worktree or selection retires the
captured command.

Native code-buffer language actions are configurable as `editor_complete`
(`Ctrl+Space`), `go_to_definition` (`F12`), `find_references` (`Shift+F12`),
`show_hover` and `show_diagnostics` (unbound). Clearing or remapping these
bindings retires their original native chords. `Space` is accepted as a named
key in configured chords and the shortcut recorder.

File-surface commands are `editor_save` (`Ctrl+S`), `editor_discard_edits`,
`editor_edit`, `editor_preview` and `editor_diff` (the latter four unbound).
Their availability follows the file toolbar, including save and read-only
checks. Clearing or remapping `editor_save` retires the original `Ctrl+S`
binding. Repository commands keep ordinary fields, chat and terminal input
separate; the Keyboard table shows **Scoped** for bindings whose pane context
cannot take a terminal's keys.

PDF viewer commands `pdf_first_page`, `pdf_previous_page`, `pdf_next_page`, `pdf_last_page`, `pdf_zoom_out`, `pdf_zoom_in`, `pdf_fit` and `pdf_toggle_colors` are unbound by default. They appear in Keyboard settings; palette availability reflects the mounted PDF preview. Page and zoom limits use the same availability as the viewer toolbar.

Diff presentation commands (`diff_unified`, `diff_split`, `diff_ignore_whitespace`, `diff_working`, `diff_staged`, `diff_branch`, `diff_history`, `diff_revert`, `diff_retry`) and explicit selected-row mutation commands (`diff_stage_selection`, `diff_unstage_selection`, `diff_revert_selection`) are unbound by default. Selection commands require actual changed rows and retain the displayed source, selected rows and layout revision through palette activation. Existing confirmation and mutation handling still apply.

`stage_or_unstage_file` defaults to Ctrl+Shift+S, preferring Stage when the file has unstaged changes and otherwise Unstage. `stage_file` and `unstage_file` are separate unbound actions. These operate on the current file, including a clean native editor; the selected-row commands remain separate. Clearing or rebinding the toggle retires Ctrl+Shift+S. Toolbar hints use current configured bindings and only advertise the toggle for the direction it would perform.

Diff presentation and staging commands are configurable as `diff_unified`,
`diff_split`, `diff_ignore_whitespace`, `diff_working`, `diff_staged`,
`diff_branch`, `diff_history`, `stage_file`, `unstage_file`,
`stage_or_unstage_file`, `diff_revert` and `diff_retry`. Only
`stage_or_unstage_file` starts bound (`Ctrl+Shift+S`); clearing it retires the
old staging shortcut. Explicit selected-row actions are `diff_stage_selection`,
`diff_unstage_selection` and `diff_revert_selection`. Current-hunk actions are
`diff_stage_hunk`, `diff_unstage_hunk` and `diff_revert_hunk`; they are available
only when no rows are selected. Mutation availability uses the actual current
worktree and source/layout identity, including selection and save guards.

Browser controls are `browser_back` (`Alt+Left`), `browser_forward`
(`Alt+Right`), `browser_reload` (`Ctrl+R`, `F5`), `browser_address` (`Ctrl+L`),
`browser_stop`, `browser_open_system` and `browser_import_logins` (the last three
unbound). Remapping or clearing the registered navigation actions retires their
old aliases. Native address and other text fields keep their editing keys;
only the conventional Back/Forward action-and-chord pairs override word motion
in browser content. Browserless mode retains saved browser descriptors and
exposes the actual system-browser action, without embedded-runtime commands.

Explicit conversation actions are `chat_send`, `chat_steer`, `chat_queue`,
`chat_interrupt`, `chat_compact`, `chat_attach`, `chat_reasoning`, `chat_latest`,
`chat_take_back_queued`, `chat_cancel_queued`, `chat_toggle_steering` and
`chat_toggle_fast_mode`. They start unbound except `chat_latest` (`Ctrl+End`),
whose original hardcoded route is retired when the binding changes. `End` is
accepted by configured chords, menus and the recorder. These explicit aliases
are separate from the composer's field confirmation policy: Enter versus
Ctrl+Enter follows `enter_sends`, Ctrl+Shift+Enter selects the alternate delivery
mode while busy, and Escape cancels through the existing confirmation flow.
A pending-first-message cancellation retains that exact starting session;
newly submitted messages do not inherit an older palette result's cancellation.

Whole-area Git commands (`git_stage_all_unstaged`, `git_stage_all_untracked`,
`git_stage_all_conflicts`, `git_unstage_all_staged`, `git_discard_all_unstaged`,
`git_discard_all_untracked`) start unbound and appear in Keyboard → Git. They
require an active nonempty area without an explicit selection in that area.
Group header controls use the selected-changes commands when rows are selected.
Captured whole-area commands refuse a changed selection, and the backend reads
the complete current area when the queued operation runs. Discard retains its
confirmation.

`editor_reload` and `editor_overwrite` are unbound recovery commands in
Keyboard → Editor. They appear only for the active editor's existing disk
conflict or durability warning. Reload reads the selected file again; Overwrite
is available only when that file has an explicit save conflict. Ordinary Save
continues to preserve its disk-conflict checks.
