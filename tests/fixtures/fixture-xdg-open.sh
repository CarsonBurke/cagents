#!/bin/sh
# The native-evidence fixture's `xdg-open`, ahead of the system's on the
# app's PATH. Without a desktop portal the app opens links, such as a Work
# card's pull request, through xdg-open; a step that clicks one must not start
# the host's browser or reach the network. This records what would have opened
# in $FIXTURE/state/opened-urls, one per line, and succeeds as the real one
# does once it hands the link over.
set -eu
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
[ "$#" -eq 1 ] || {
    printf '%s\n' 'Usage: xdg-open { file | URL }' >&2
    exit 1
}
mkdir -p -- "$fixture/state"
printf '%s\n' "$1" >>"$fixture/state/opened-urls"
