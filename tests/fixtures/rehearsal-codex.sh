#!/bin/sh
# Offline Codex for rehearse-update. Install this as ROOT/bin/codex, alongside
# ROOT/libexec/codex/{rehearsal-codex,codex-tool-rows,terminal-escape-codex}.py.
# Derive ROOT from the executable because native launches remove CAGENTS_*
# environment values.
set -eu
. "$(dirname -- "$(readlink -f -- "$0")")/lib/fixture-home.sh"
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
require_fixture_home CODEX_HOME "${CODEX_HOME:-$fixture/home/.codex}"
CODEX_HOME=$fixture_home_resolved
export CODEX_HOME
mkdir -p -- "$CODEX_HOME"
export CODEX_HOME
CAGENTS_TOOL_ROWS_FIXTURE_ROOT=$fixture
export CAGENTS_TOOL_ROWS_FIXTURE_ROOT
exec python3 -I "$fixture/libexec/codex/rehearsal-codex.py" "$@" 2>>"$fixture/logs/codex-provider.stderr.log"
