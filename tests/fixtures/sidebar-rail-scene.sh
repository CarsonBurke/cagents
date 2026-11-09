#!/bin/bash
# A populated sidebar for native evidence of the navigation rail and the
# expanded sidebar: ten projects of mixed name lengths, channels and worktrees
# in each, and live agents in every attention state the rail draws.
#
# Run from a checkout on the build host, with CAGENTS_EVIDENCE_TASK naming a
# scripts/native-evidence task (see "Native evidence" in docs/development.md):
#
#   tests/fixtures/sidebar-rail-scene.sh seed       # once, then save-seed
#   tests/fixtures/sidebar-rail-scene.sh unreachable # once, after save-seed
#   tests/fixtures/sidebar-rail-scene.sh theme dark # or light; edits the seed
#   scripts/native-evidence start before --width 1100
#   tests/fixtures/sidebar-rail-scene.sh live       # each session
#
# `seed` needs a running session, as `cli` does; it opens the projects and
# creates their channels and worktrees, which the seed keeps. `live` starts
# synthetic Claude agents (tests/fixtures/chat-composer-claude.sh) whose state
# is the point of the scene, so it runs again in every session: a stopped
# session keeps no running agent. It writes the provider's
# fixture-frames.jsonl before each agent starts and waits for that agent's
# first turn to read it:
#
#   working   no frames: the fixture holds its first turn busy
#   unread    a successful result ends the turn while its channel is not open
#   failed    an error result ends the turn
#   waiting   a tool permission request, held open
#
# `unreachable` moves one project's folder away in the seed, so every later
# session finds that project missing as it loads the sidebar.
#
# `live` also puts a `git` in front of the real one on the session's PATH
# that refuses to remove billing-service's linked worktrees, slowly:
# deleting one from the sidebar shows it deleting for 20 seconds, then
# failed. Every other command reaches the real Git. Nothing in Git itself
# serves: the delete dialog refuses a locked worktree both as it opens and as
# it is confirmed, and Git forgets a worktree it could only partly remove.
#
# The result frames carry no session_id: the app refuses a result from any
# conversation but the agent's own, and the fixture names that one itself.
#
# Half the projects have a github.com remote whose owner has an avatar seeded
# for the fixture's offline forge (docs/fixtures.md, "An offline GitHub"), so
# the rail mixes avatar images with initials. The avatars are generated
# identicons: no real account's image is involved.
set -euo pipefail

here=$(cd -- "$(dirname -- "$0")/../.." && pwd)
# The tool may come from another checkout, so this scene can be copied out of
# one whose build must stay unchanged.
evidence() { "${CAGENTS_EVIDENCE_TOOL:-$here/scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
seed=$CAGENTS_EVIDENCE_TASK/fixture-seed
projects=$fixture/projects

# Owners whose projects draw an avatar; the other projects keep no remote.
declare -A owners=(
    [atlas]=northwind-labs
    [cagents]=octo-org
    [glacier-ml]=glacier-ai
    [harbor]=harborworks
    [journal]=quill-studio
)

# name: channels, then worktrees.
scene=(
    "atlas:Release notes,Flaky login test:feature-search"
    "billing-service:Invoice rounding,Stripe webhooks,Tax export:fix-proration,refactor-ledger"
    "cagents:Sidebar rail,Transcript pipeline,Channel speed,Usage truth:rail-badges,channel-open"
    "design-system:Tokens:dark-palette"
    "edge-proxy:TLS renewals:"
    "ferry:Onboarding copy,Crash on resume:ios-push"
    "glacier-ml:Eval harness,Tokenizer drift:ablation-lr"
    "harbor:Docs site:"
    "infra-terraform-modules-shared:Cost report:"
    "journal:Sync conflicts,Markdown export:offline-mode"
)

cli_json() { evidence cli --json "$@"; }

channel_id() {
    # The newest channel in the repository with this title.
    cli_json channel list --repository "$1" |
        jq -r --arg title "$2" '[.. | objects | select(.title? == $title) | (.id // .channel_id)] | first // empty'
}

# A GitHub-style identicon: a mirrored 5x5 grid in one colour, as a PNG.
identicon() {
    python3 -I - "$1" "$2" <<'PY'
import hashlib, struct, sys, zlib
owner, path = sys.argv[1], sys.argv[2]
digest = hashlib.sha256(owner.encode()).digest()
colour = (64 + digest[0] % 160, 64 + digest[1] % 160, 64 + digest[2] % 160)
cells = [[digest[3 + row * 3 + min(col, 4 - col)] % 2 == 0 for col in range(5)] for row in range(5)]
size, cell, margin = 64, 10, 7
rows = []
for y in range(size):
    line = bytearray([0])
    for x in range(size):
        cx, cy = (x - margin) // cell, (y - margin) // cell
        on = 0 <= cx < 5 and 0 <= cy < 5 and cells[cy][cx]
        line += bytes(colour if on else (240, 240, 240))
    rows.append(bytes(line))
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(b"".join(rows))) + chunk(b"IEND", b"")
open(path, "wb").write(png)
PY
}

cmd_seed() {
    mkdir -p "$projects" "$fixture/avatars" "$fixture/home/.config"
    local owner
    for owner in "${owners[@]}"; do
        identicon "$owner" "$fixture/avatars/$owner.image"
    done
    # The forge answers only once this exists, and the avatars above must be
    # in place before it does.
    touch "$fixture/home/.config/fixture-forge"
    local entry name channels worktrees project title branch
    for entry in "${scene[@]}"; do
        IFS=: read -r name channels worktrees <<<"$entry"
        project=$projects/$name
        if [ ! -e "$project/.git" ]; then
            evidence env sh -c '
                set -e
                export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
                git init --quiet --initial-branch=main "$1"
                cd "$1"
                printf "# %s\n" "${1##*/}" > README.md
                mkdir -p src && printf "fn main() {}\n" > src/main.rs
                git add -A
                git -c user.name=Fixture -c user.email=fixture@example.invalid commit --quiet -m "Initial commit"
            ' sh "$project"
            if [ -n "${owners[$name]:-}" ]; then
                evidence env git -C "$project" remote add origin "https://github.com/${owners[$name]}/$name"
            fi
            IFS=, read -r -a branches <<<"$worktrees"
            for branch in "${branches[@]}"; do
                [ -n "$branch" ] || continue
                evidence env sh -c '
                    set -e
                    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
                    git -C "$1" worktree add --quiet -b "$2" "$1-$2"
                    # Uncommitted work, so the expanded rows carry a changes badge.
                    printf "draft\n" >> "$1-$2/README.md"
                    printf "notes\n" > "$1-$2/NOTES.md"
                ' sh "$project" "$branch"
            done
        fi
        evidence cli project open "$project"
        IFS=, read -r -a titles <<<"$channels"
        for title in "${titles[@]}"; do
            [ -n "$(channel_id "$project" "$title")" ] ||
                evidence cli channel spawn --repository "$project" --title "$title" >/dev/null
        done
    done
}

frames=$fixture/home/.claude/fixture-frames.jsonl

frames_for() {
    case $1 in
        working) rm -f -- "$frames" ;;
        unread)
            printf '%s\n' \
                '{"type":"result","uuid":"scene-result-@TURN@","subtype":"success","is_error":false,"result":"Done."}' \
                '{"_end":true}' >"$frames"
            ;;
        failed)
            printf '%s\n' \
                '{"type":"result","uuid":"scene-result-@TURN@","subtype":"error_during_execution","is_error":true,"errors":["The test runner crashed."]}' \
                '{"_end":true}' >"$frames"
            ;;
        waiting)
            printf '%s\n' \
                '{"type":"control_request","request_id":"permission-@TURN@","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-@TURN@","input":{"command":"rm -rf target","description":"Clear the build cache"}}}' \
                '{"_sleep":3600}' >"$frames"
            ;;
    esac
}

# spawn STATE PROJECT CHANNEL-TITLE [DIRECTORY]
spawn() {
    local state=$1 project=$projects/$2 title=$3 directory=${4:-}
    local channel
    channel=$(channel_id "$project" "$title")
    [ -n "$channel" ] || { printf 'no channel %s in %s\n' "$title" "$2" >&2; return 1; }
    frames_for "$state"
    if ! evidence cli agent spawn --provider claude --model haiku --surface chat \
        --repository "$project" ${directory:+--directory "$directory"} --channel "$channel" \
        --prompt "Fixture: $state" >/dev/null; then
        # The daemon caps one caller's unsettled runs; say which are open.
        cli_json agent list | jq -r '.data.agents[] | "open: \(.state) \(.agent.spec.target.directory | split("/") | last)"' >&2
        return 1
    fi
    # Each agent reads the frames as its first turn starts; give it that turn
    # before the next state replaces them.
    sleep 4
}

cmd_live() {
    # Unread counts: two digits in one channel, one elsewhere. A finished run
    # leaves the daemon's cap on running agents, so these may be many.
    local n
    for n in $(seq 12); do
        spawn unread billing-service "Invoice rounding"
    done
    spawn unread journal "Markdown export"
    spawn failed glacier-ml "Eval harness"
    spawn failed glacier-ml "Eval harness"
    spawn waiting cagents "Sidebar rail"
    spawn waiting ferry "Crash on resume"
    spawn working cagents "Transcript pipeline"
    spawn working cagents "Channel speed"
    spawn working atlas "Flaky login test"
    spawn working design-system "Tokens" "$projects/design-system-dark-palette"
    # Working with unread news beside it: both corners at once.
    spawn unread edge-proxy "TLS renewals"
    spawn working edge-proxy "TLS renewals"
    rm -f -- "$frames"
    refuse_removal
}

refuse_removal() {
    local real
    real=$(PATH=/usr/local/bin:/usr/bin command -v git)
    # The fixture's bin comes first on the session's PATH, and the app looks
    # Git up there each time it runs it.
    cat >"$fixture/bin/git" <<EOF
#!/bin/sh
case " \$* " in
    *" worktree remove "*"/billing-service-"*)
        sleep 20
        echo "error: fixture: refusing to remove this worktree" >&2
        exit 1
        ;;
esac
exec $real "\$@"
EOF
    chmod 755 "$fixture/bin/git"
}

cmd_unreachable() {
    # A folder moved while a session runs is noticed only at the next load,
    # so this edits the seed every session starts from.
    [ -d "$seed" ] || { echo "save the seed first" >&2; return 1; }
    [ ! -d "$seed/projects/harbor" ] || mv "$seed/projects/harbor" "$seed/projects/harbor.away"
}

cmd_theme() {
    # The seed holds no settings until something is changed in Settings, and
    # a document naming only its schema and the theme leaves the rest at their
    # defaults.
    local key=$1 settings=$seed/config/cagents-development/settings.toml
    [ -d "$seed" ] || { echo "save the seed first" >&2; return 1; }
    mkdir -p "${settings%/*}"
    if [ -f "$settings" ] && grep -q '^app_theme' "$settings"; then
        sed -i "s/^app_theme = .*/app_theme = \"$key\"/" "$settings"
    elif [ -f "$settings" ]; then
        sed -i "/^schema_version/a app_theme = \"$key\"" "$settings"
    else
        printf 'schema_version = 3\napp_theme = "%s"\n' "$key" >"$settings"
    fi
}

case ${1:-} in
    seed) cmd_seed ;;
    live) cmd_live ;;
    unreachable) cmd_unreachable ;;
    theme) cmd_theme "${2:?dark or light}" ;;
    *) sed -n '2,37p' "$0" >&2; exit 2 ;;
esac
