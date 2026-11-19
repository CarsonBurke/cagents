#!/bin/bash
# A Claude conversation full of pictures, for native evidence of how the app
# shows images in a transcript (#3153). Run on the build host, with
# CAGENTS_EVIDENCE_TASK naming a scripts/native-evidence task:
#
#   scripts/native-evidence start before --width 1500 --height 1000
#   tests/fixtures/image-gallery-scene.sh open          # write frames, start the chat
#   tests/fixtures/image-gallery-scene.sh shots dark-wide  # scroll, capture each state
#   scripts/native-evidence stop
#   scripts/native-evidence save-seed                   # the chat reopens next time
#   tests/fixtures/image-gallery-scene.sh theme light   # between sessions
#   scripts/native-evidence start after --width 1500 --height 1000
#   tests/fixtures/image-gallery-scene.sh open
#   tests/fixtures/image-gallery-scene.sh shots light-wide
#
# The narrowest window the app allows (--width 900, with the sidebar and the
# right panel open) leaves a narrow transcript; set IMAGE_GALLERY_SCROLL_AT to
# a point over it, such as 430,500.
#
# The chat's one turn replays frames the way Claude Code 2.1 streams them
# (see tests/fixtures/claude-read-images-live.jsonl, recorded from the real
# provider):
#
#   reads   two screenshots read in parallel, each image in its Read's tool
#           result and followed by Claude's synthetic note on how it scaled
#           the picture for the model (the user's report in #3153)
#   pasted  a message from the reader with two pasted screenshots
#   single  one tall image read alone
#   five    a message asking for five breakpoints, and five Reads
#
# The images are drawn with ImageMagick and are synthetic. No model, account
# or network is contacted.
set -euo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
ne() { "${CAGENTS_EVIDENCE_TOOL:-$here/../../scripts/native-evidence}" "$@"; }
: "${CAGENTS_EVIDENCE_TASK:?name the native-evidence task}"
fixture=$CAGENTS_EVIDENCE_TASK/fixture
work=$CAGENTS_EVIDENCE_TASK/image-gallery-scene

picture() { # NAME WIDTH HEIGHT FROM TO LABEL
    local file=$work/$1
    [ -s "$file" ] && return
    magick -size "$2x$3" "gradient:$4-$5" \
        -fill '#ffffff22' -draw "roundrectangle 40,40 $(($2 - 40)),110 12,12" \
        -fill '#ffffff18' -draw "roundrectangle 40,140 $(($2 / 3)),$(($3 - 40)) 12,12" \
        -fill '#ffffff10' -draw "roundrectangle $(($2 / 3 + 30)),140 $(($2 - 40)),$(($3 - 40)) 12,12" \
        -fill white -pointsize $(($3 / 14)) -gravity center -annotate +0+0 "$6" \
        "png:$file"
}

frames() {
    mkdir -p "$work"
    # Each is the size Claude shows the model; reads of larger files say so.
    picture proposed-slow.png 1568 1045 '#1e293b' '#475569' 'Proposed: slow'
    picture proposed-fast.png 1568 1045 '#14532d' '#15803d' 'Proposed: fast'
    picture laptop.png 1440 900 '#312e81' '#6366f1' 'On my laptop'
    picture phone.png 780 1688 '#3b0764' '#9333ea' 'On my phone'
    picture narrow.png 600 1200 '#7c2d12' '#ea580c' 'Narrow'
    picture bp-360.png 720 1440 '#0f172a' '#0369a1' '360 px'
    picture bp-768.png 1152 1536 '#164e63' '#0891b2' '768 px'
    picture bp-1024.png 1536 1152 '#365314' '#65a30d' '1024 px'
    picture bp-1280.png 1568 980 '#4a044e' '#a21caf' '1280 px'
    picture bp-1600.png 1568 882 '#450a0a' '#b91c1c' '1600 px'
    /usr/bin/python3 -I - "$work" >"$work/frames.jsonl" <<'PY'
import base64, json, struct, sys
work = sys.argv[1]
docs = "/fixture/project/docs/"
def size(name):
    with open(f"{work}/{name}", "rb") as f:
        head = f.read(24)
    return struct.unpack(">II", head[16:24])
def data(name):
    with open(f"{work}/{name}", "rb") as f:
        return base64.b64encode(f.read()).decode()
def emit(frame):
    print(json.dumps(frame, separators=(",", ":")))
def assistant(key, content):
    emit({"type": "assistant", "uuid": f"gallery-{key}-@TURN@", "parent_tool_use_id": None,
          "message": {"id": f"gallery-{key}-@TURN@", "role": "assistant", "content": content}})
def user(key, content, **extra):
    emit({"type": "user", "uuid": f"gallery-{key}-@TURN@", "parent_tool_use_id": None,
          "message": {"role": "user", "content": content}, **extra})
def call(key, name):
    return {"type": "tool_use", "id": f"read-{key}-@TURN@", "name": "Read",
            "input": {"file_path": docs + name}}
def result(key, name, scaled=True):
    shown = size(name)
    width, height = (round(shown[0] * 1.25), round(shown[1] * 1.25)) if scaled else shown
    user(f"result-{key}", [{"tool_use_id": f"read-{key}-@TURN@", "type": "tool_result",
         "content": [{"type": "image", "source": {"type": "base64", "data": data(name), "media_type": "image/png"}}]}],
         tool_use_result={"type": "image", "file": {"type": "image/png", "dimensions": {
             "originalWidth": width, "originalHeight": height,
             "displayWidth": shown[0], "displayHeight": shown[1]}}})
    if shown != (width, height):
        user(f"note-{key}", [{"type": "text", "text":
             f"[Image: original {width}x{height}, displayed at {shown[0]}x{shown[1]}. "
             f"Multiply coordinates by {width / shown[0]:.2f} to map to original image.]"}],
             isSynthetic=True)
def text(key, words):
    assistant(key, [{"type": "text", "text": words}])
def image(name):
    return {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data(name)}}

# reads: two screenshots read in parallel, as the reader reported.
assistant("reads", [call("slow", "proposed-slow.png")])
assistant("reads", [call("fast", "proposed-fast.png")])
result("slow", "proposed-slow.png")
result("fast", "proposed-fast.png")
text("reads-answer", "The fast layout keeps the inspector visible; the slow one hides it behind a second panel.")
# pasted: the reader answers with screenshots of their own.
user("pasted", [{"type": "text", "text": "Here's how the fast one looks on my laptop and my phone."},
                image("laptop.png"), image("phone.png")])
# single: one tall image.
assistant("single", [call("narrow", "narrow.png")])
result("narrow", "narrow.png", scaled=False)
text("single-answer", "At the narrow width the toolbar wraps onto a second line.")
# five: five breakpoints read together.
user("five", [{"type": "text", "text": "Check all five breakpoints."}])
names = ["bp-360.png", "bp-768.png", "bp-1024.png", "bp-1280.png", "bp-1600.png"]
for name in names:
    assistant("five", [call(name[:-4], name)])
for name in names:
    result(name[:-4], name)
text("five-answer", "Every breakpoint keeps the composer in view; 360 px wraps the tab strip.")
PY
}

cmd_theme() { # Between sessions: the fixture's and any saved seed's settings.
    local key=${1:?dark or light} root settings
    for root in "$fixture" "$CAGENTS_EVIDENCE_TASK/fixture-seed"; do
        [ -d "$root" ] || continue
        settings=$root/config/cagents-development/settings.toml
        mkdir -p "${settings%/*}"
        if [ -f "$settings" ] && grep -q '^app_theme' "$settings"; then
            sed -i "s/^app_theme = .*/app_theme = \"$key\"/" "$settings"
        elif [ -f "$settings" ]; then
            sed -i "/^schema_version/a app_theme = \"$key\"" "$settings"
        else
            printf 'schema_version = 3\napp_theme = "%s"\n' "$key" >"$settings"
        fi
    done
}

cmd_open() {
    frames
    ne env sh -c 'mkdir -p "$CAGENTS_EVIDENCE_FIXTURE/home/.claude"; cat > "$CAGENTS_EVIDENCE_FIXTURE/home/.claude/fixture-frames.jsonl"' <"$work/frames.jsonl"
    ne cli project open "$fixture/project" >/dev/null
    ne cli --json agent spawn --provider claude --model haiku --surface chat \
        --repository "$fixture/project" --prompt 'Fixture: compare the proposed layouts.' >/dev/null
    local row
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        sleep 3
        row=$(ne drive observe 2>/dev/null | awk '/ compare$|Fixture:|New channel/ { if (match($0, /[0-9]+, *[0-9]+ +[0-9]+x[0-9]+/)) { print substr($0, RSTART, RLENGTH); exit } }' |
            tr ',x' '  ' | awk '{printf "%d,%d", $1+$3/2, $2+$4/2}')
        [ -n "$row" ] || continue
        ne drive click "$row" >/dev/null
        sleep 3
        if ne drive observe 2>/dev/null | rg -q 'breakpoint|composer in view'; then
            return 0
        fi
    done
    printf '%s\n' 'image-gallery-scene: the conversation did not open' >&2
    return 1
}

# The top of TEXT on screen, if it is.
top_of() {
    ne drive observe 2>/dev/null | rg -- "$1" |
        awk '{ if (match($0, /[0-9]+, *[0-9]+ +[0-9]+x[0-9]+/)) { split(substr($0, RSTART, RLENGTH), p, /[, x]+/); print p[2]; exit } }'
}

# Scroll the transcript, from its top, until TEXT is on screen and no lower
# than TOP pixels from the window's top.
reveal() {
    local text=$1 top=${2:-120} at=${IMAGE_GALLERY_SCROLL_AT:-700,500} y
    ne drive scroll --at "$at" v -- -80 >/dev/null
    sleep 1
    for _ in $(seq 1 60); do
        y=$(top_of "$text")
        if [ -n "$y" ] && [ "$y" -le "$top" ]; then
            return 0
        fi
        ne drive scroll --at "$at" v -- 1 >/dev/null
        sleep 0.4
    done
    [ -n "$y" ] && return 0
    printf 'image-gallery-scene: not found: %s\n' "$text" >&2
    return 1
}

# Take shot NAME with the pointer out of the way, so no hover or tooltip
# shows in it.
still() {
    ne drive move 1,1 >/dev/null
    sleep 1.5
    ne shot "$1"
}

# A point on the first picture of the five breakpoints: the middle of the
# control named for it, or a little above its caption.
first_breakpoint() {
    local below
    below=$(top_of 'five breakpoints')
    ne drive observe 2>/dev/null | rg -- 'bp-360\.png|Click to enlarge' |
        awk -v below="${below:-0}" '{ if (match($0, /[0-9]+, *[0-9]+ +[0-9]+x[0-9]+/)) {
            split(substr($0, RSTART, RLENGTH), p, /[, x]+/)
            if (p[2] <= below) next
            # The picture itself, named for its file, or its caption.
            if (p[4] > 60) printf "%d,%d", p[1] + p[3] / 2, p[2] + p[4] / 2
            else printf "%d,%d", p[1] + 60, p[2] - 80
            exit } }'
}

cmd_shots() { # LABEL, such as dark-wide, prefixes each shot's name
    local label=${1:?a label for the shots, such as dark-wide}
    ne drive move 1,1 >/dev/null
    reveal 'compare the proposed' 900 && still "$label-two-reads"
    reveal 'on my laptop' 500 && still "$label-pasted"
    reveal 'docs/narrow' && still "$label-single"
    reveal 'five breakpoints' && still "$label-five"
    # The first breakpoint, pointed at, then opened, then the next picture
    # in the viewer.
    local at
    at=$(first_breakpoint)
    [ -n "$at" ] || { printf '%s\n' 'image-gallery-scene: no breakpoint picture' >&2; return 1; }
    ne drive move "$at" >/dev/null
    sleep 1.5
    ne shot "$label-hover"
    ne drive click "$at" >/dev/null
    sleep 2
    ne drive move 1,1 >/dev/null
    sleep 1
    ne shot "$label-viewer"
    ne drive key Right >/dev/null
    sleep 2
    ne shot "$label-viewer-next"
    ne drive key Escape >/dev/null
}

case ${1:-} in
    theme) cmd_theme "${2:-}" ;;
    frames) frames && printf '%s\n' "$work/frames.jsonl" ;;
    open) cmd_open ;;
    shots) cmd_shots "${2:-}" ;;
    *) sed -n '2,26p' "$0" >&2; exit 2 ;;
esac
