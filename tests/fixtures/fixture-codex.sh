#!/bin/sh
# The native-evidence fixture's `codex`: one of the offline Codex app-server
# peers in tests/fixtures, which `scripts/native-evidence start SIDE --codex
# PEER` copies into $FIXTURE/libexec/codex and names in its `peer` file. A
# session started without --codex has no `codex` on PATH, so the app sees Codex
# as not installed, as before.
#
#   tool-rows        codex-tool-rows.py: each prompt plays one turn of tool,
#                    collaboration and event items, started then completed.
#   child-errors     codex-child-errors.py: subagent diagnostics and failed turns.
#   terminal-escape  terminal-escape-codex.py: streamed terminal escapes.
#
# Each peer wants a private root (mode 0700, the fixture), CODEX_HOME beneath
# it and a private XDG_RUNTIME_DIR; this supplies the root and CODEX_HOME.
set -eu

# Installed as $FIXTURE/bin/codex: its own location is the root, since neither
# the app nor the daemon promises to pass the fixture's variables on.
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
peers=$fixture/libexec/codex
peer=$(cat -- "$peers/peer" 2>/dev/null) || peer=
case $peer in
    tool-rows) script=codex-tool-rows.py root=CAGENTS_TOOL_ROWS_FIXTURE_ROOT ;;
    child-errors) script=codex-child-errors.py root=CAGENTS_CHILD_ERROR_FIXTURE_ROOT ;;
    terminal-escape) script=terminal-escape-codex.py root=CAGENTS_ESCAPE_FIXTURE_ROOT ;;
    *)
        echo "codex: the fixture has no Codex peer; start the session with --codex tool-rows, child-errors or terminal-escape" >&2
        exit 127
        ;;
esac
# The app may name an account's home; otherwise Codex's own default.
CODEX_HOME=${CODEX_HOME:-$fixture/home/.codex}
mkdir -p -- "$CODEX_HOME"
export CODEX_HOME
# $root is one of the names above.
eval "$root=\$fixture; export $root"
exec python3 -I "$peers/$script" "$@"
