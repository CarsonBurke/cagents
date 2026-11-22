#!/bin/bash
# Native measurement steps for issue #3612. Run only from the workstation:
#
# scripts/native-evidence --remote fw16 pair \
#   --before origin/main --after perf/daemon-writes --steps \
#   scripts/needs-native-run-session-writes.sh
#
# The runner supplies `evidence`, `task`, `tool`, and SIDE. This measures the
# real UI/session writer at the same streaming, idle, and reconnect states on
# main and this head. It deliberately records process counters rather than
# payload contents.
set -euo pipefail

metrics="$task/metrics"
mkdir -p "$metrics"
runtime="$task/rt"

fixture_pids() {
    grep -lzx "XDG_RUNTIME_DIR=$runtime" /proc/[0-9]*/environ 2>/dev/null |
        cut -d/ -f3 || true
}

pid_named() {
    local needle=$1 pid command
    for pid in $(fixture_pids); do
        command=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || :)
        case $command in *"$needle"*) printf '%s\n' "$pid"; return 0 ;; esac
    done
    return 1
}

io_field() {
    awk -v field="$2" '$1 == field ":" { print $2 }' "/proc/$1/io"
}

sample_io() {
    local label=$1 seconds=$2 daemon ui d_wchar_start u_wchar_start d_syscw_start u_syscw_start
    local d_wchar_end u_wchar_end d_syscw_end u_syscw_end
    daemon=$(pid_named cagentsd)
    ui=$(pid_named cagents-ui)
    d_wchar_start=$(io_field "$daemon" wchar)
    u_wchar_start=$(io_field "$ui" wchar)
    d_syscw_start=$(io_field "$daemon" syscw)
    u_syscw_start=$(io_field "$ui" syscw)
    sleep "$seconds"
    d_wchar_end=$(io_field "$daemon" wchar)
    u_wchar_end=$(io_field "$ui" wchar)
    d_syscw_end=$(io_field "$daemon" syscw)
    u_syscw_end=$(io_field "$ui" syscw)
    printf '%s\tseconds=%s\tdaemon_wchar=%s\tdaemon_syscw=%s\tui_wchar=%s\tui_syscw=%s\n' \
        "$label" "$seconds" \
        "$((d_wchar_end - d_wchar_start))" "$((d_syscw_end - d_syscw_start))" \
        "$((u_wchar_end - u_wchar_start))" "$((u_syscw_end - u_syscw_start))" \
        >>"$metrics/$SIDE-session-io.tsv"
}

# The fixture is offline and deterministic. Retain the same seeded workspace
# for both sides of the pair; its four streams model concurrent real agents
# without creating a synthetic CPU/load loop.
evidence cli project open '{fixture}/project' >/dev/null
evidence env sh -c 'printf "0.03\\n24\\n" >"$HOME/.claude/fixture-stream-reply"'
for number in 1 2 3 4; do
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --name "session-write-$number" --repository '{fixture}/project' \
        --prompt "Stream a detailed progress report number $number." \
        >"$metrics/$SIDE-spawn-$number.json"
done

evidence drive wait 'session-write-1' --timeout 30 >/dev/null
evidence drive click 'session-write-1' >/dev/null
sleep 5
sample_io streaming 30

for number in 1 2 3 4; do
    evidence cli agent interrupt "session-write-$number" >/dev/null || :
done
sleep 5
sample_io idle 30

# A second window takes a WatchWorkspaceModel snapshot, then receives the
# settled model while no chat stream is active. This is the publication path
# that previously could schedule duplicate session snapshots.
evidence open-window session-write-peer >/dev/null
sleep 5
sample_io reconnect_idle 30

printf 'side=%s\trevision=%s\n' "$SIDE" "$(git -C "$tool" rev-parse HEAD)" \
    >"$metrics/$SIDE-revision.tsv"
