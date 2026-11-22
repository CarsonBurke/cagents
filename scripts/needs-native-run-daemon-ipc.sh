#!/bin/bash
# Native measurement steps for daemon/IPC work.  Run through
# `scripts/native-evidence --remote fw16 ... pair --steps THIS_FILE`, never
# from an agent-hosted shell.  The runner supplies `evidence`, `task`, `tool`,
# and SIDE (before or after).  See the invocation and result checklist below.
#
# This deliberately has no product assertions: it records the baseline needed
# to choose a publication/serialization change, and keeps the stream, idle,
# reconnect-snapshot, and retained-actor request measurements comparable.
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

ticks() {
    # The command name is parenthesized and can contain spaces, so remove it
    # before selecting utime and stime (fields 14 and 15 in /proc/PID/stat).
    sed -E 's/^[0-9]+ \(.*\) //' "/proc/$1/stat" | awk '{print $12 + $13}'
}

sample_cpu() {
    local label=$1 seconds=$2 daemon ui start_daemon start_ui end_daemon end_ui hz
    daemon=$(pid_named cagentsd)
    ui=$(pid_named cagents-ui)
    hz=$(getconf CLK_TCK)
    start_daemon=$(ticks "$daemon")
    start_ui=$(ticks "$ui")
    sleep "$seconds"
    end_daemon=$(ticks "$daemon")
    end_ui=$(ticks "$ui")
    printf '%s\tseconds=%s\tdaemon_pid=%s\tdaemon_cpu_ms=%s\tui_pid=%s\tui_cpu_ms=%s\n' \
        "$label" "$seconds" "$daemon" \
        "$(( (end_daemon - start_daemon) * 1000 / hz ))" "$ui" \
        "$(( (end_ui - start_ui) * 1000 / hz ))" >>"$metrics/$SIDE-cpu.tsv"
}

capture_socket_syscalls() {
    # strace is a short, separate traffic pass: do not use it for CPU results.
    # It records Unix-socket read/write lengths with their socket inode; its
    # output gives bytes and frame frequency without content.  ptrace_scope can
    # forbid attach on a host; preserve that refusal as evidence rather than
    # silently claiming a byte count.
    local label=$1 seconds=$2 daemon trace_pid
    daemon=$(pid_named cagentsd)
    if ! command -v strace >/dev/null; then
        printf '%s\tstrace=unavailable\n' "$label" >>"$metrics/$SIDE-traffic.tsv"
        return 0
    fi
    timeout "$((seconds + 3))" strace -ff -qq -ttt -T -yy \
        -e trace=read,write,recvfrom,sendto,recvmsg,sendmsg \
        -p "$daemon" -o "$metrics/$SIDE-$label-daemon.syscalls" &
    trace_pid=$!
    sleep "$seconds"
    wait "$trace_pid" || status=$?
    # timeout's expected exit makes the end of this bounded capture explicit.
    printf '%s\tseconds=%s\tstrace_status=%s\tpid=%s\n' \
        "$label" "$seconds" "${status:-0}" "$daemon" >>"$metrics/$SIDE-traffic.tsv"
}

measure_retained_request() {
    local begin end elapsed
    begin=$(date +%s%3N)
    # The streamed first turn was interrupted below. A second request therefore
    # exercises the daemon's retained actor under the same loaded workspace.
    evidence cli agent send ipc-stream-1 'Reply with the word retained.' --wait --timeout 15 \
        >"$metrics/$SIDE-retained-request.json"
    end=$(date +%s%3N)
    elapsed=$((end - begin))
    printf 'retained_request\twall_ms=%s\n' "$elapsed" >>"$metrics/$SIDE-retained.tsv"
}

# Preserve a large, deterministic workspace model produced with
# `native-evidence seed --profile realistic --seed 1` before this paired run.
evidence cli project open '{fixture}/project' >/dev/null
evidence env sh -c 'printf "0.03\n24\n" >"$HOME/.claude/fixture-stream-reply"'

# Four independent streams are enough to publish real chat activity while the
# UI observes a realistic sidebar/workspace. The fixture remains offline.
for number in 1 2 3 4; do
    evidence cli --json agent spawn --provider claude --model haiku --surface chat \
        --name "ipc-stream-$number" --repository '{fixture}/project' \
        --prompt "Stream a detailed daemon IPC progress report number $number." \
        >"$metrics/$SIDE-spawn-$number.json"
done

# Put one active stream on screen, then leave the pointer still for the two
# CPU passes. UI trace and workspace trace are enabled by the caller.
evidence drive wait 'ipc-stream-1' --timeout 30 >/dev/null
evidence drive click 'ipc-stream-1' >/dev/null
evidence drive hover 1,1 >/dev/null
sleep 5

sample_cpu streaming 30
capture_socket_syscalls streaming 10

# A second window receives precisely the reconnect WatchWorkspaceModel
# snapshot. It is opened while stream activity is stopped so the raw socket
# capture isolates the snapshot bytes from chat publications.
for number in 1 2 3 4; do
    evidence cli agent interrupt "ipc-stream-$number" >/dev/null || :
done
sleep 5
sample_cpu idle 30
capture_socket_syscalls idle 10

capture_socket_syscalls reconnect_snapshot 8 &
traffic_capture=$!
sleep 1
evidence open-window ipc-peer >/dev/null
wait "$traffic_capture" || :

measure_retained_request

# Pair brings logs and this metrics directory back in the task.  The lead
# compares before/after records, then separately totals each .syscalls file by
# direction and socket inode; no payload text is collected.
printf 'side=%s\nsource=%s\n' "$SIDE" "$(git -C "$tool" rev-parse HEAD)" \
    >"$metrics/$SIDE-revision.tsv"
