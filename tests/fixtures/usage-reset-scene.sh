#!/bin/bash
# Prepare invented reset data in a running native-evidence fixture.
# seed once before save-seed; serve again after each start restores the seed.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")/../.." && pwd)
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/scripts/native-evidence}" "$@"; }
command=${1:?usage: usage-reset-scene.sh seed|serve|mode PROVIDER MODE}
case $command in
    seed)
        evidence env sh -c 'exec /usr/bin/python3 "$1" seed "$CAGENTS_EVIDENCE_FIXTURE"' sh "$here/tests/fixtures/usage-reset-fixture.py"
        ;;
    serve)
        # The private HOME/runtime let native-evidence stop attribute this
        # process. Redirect both outputs so an SSH caller never holds its pipe.
        evidence env sh -c '
            root=$CAGENTS_EVIDENCE_FIXTURE
            if [ -f "$root/usage-reset-server.pid" ] &&
                grep -qFxz "HOME=$root/home" "/proc/$(cat "$root/usage-reset-server.pid")/environ" 2>/dev/null &&
                grep -qFxz "$1" "/proc/$(cat "$root/usage-reset-server.pid")/cmdline" 2>/dev/null; then
                echo "fixture reset server is already running" >&2
                exit 2
            fi
            rm -f -- "$root/home/.claude/fixture-reset-api.json"
            /usr/bin/python3 "$1" serve "$root" >"$root/usage-reset-server.json" 2>"$root/usage-reset-server.log" &
            echo $! >"$root/usage-reset-server.pid"
            for attempt in $(seq 100); do
                [ -f "$root/home/.claude/fixture-reset-api.json" ] && exit 0
                kill -0 "$!" 2>/dev/null || break
                sleep 0.05
            done
            cat "$root/usage-reset-server.log" >&2
            exit 2
        ' sh "$here/tests/fixtures/usage-reset-fixture.py"
        ;;
    mode)
        provider=${2:?name claude or codex}
        mode=${3:?name success, error, disconnect or unconfirmed}
        [[ $provider =~ ^(claude|codex)$ && $mode =~ ^(success|error|disconnect|unconfirmed)$ ]] || exit 2
        [[ $provider == claude || $mode != unconfirmed ]] || { echo "unconfirmed is a Claude-only mode" >&2; exit 2; }
        evidence env /usr/bin/python3 -c '
import json, os, pathlib, sys
root, provider, mode = pathlib.Path(os.environ["CAGENTS_EVIDENCE_FIXTURE"]), sys.argv[1], sys.argv[2]
assert (root / ".cagents-reset-fixture").is_file()
path = root / "home" / ("." + provider) / "fixture-reset-state.json"
state = json.loads(path.read_text())
assert state["fixture"] == "cagents-fixture-only"
state["mode"] = mode
path.write_text(json.dumps(state))
' "$provider" "$mode"
        ;;
    *) echo "unknown reset scene command: $command" >&2; exit 2 ;;
esac
