#!/bin/sh
# foreach enters each populated child and clears the parent repository context.
# The executable stays data even when its path contains spaces or shell syntax.
set -eu
location=$("$CAGENTS_PR_GIT" --no-optional-locks -c core.fsmonitor=false \
    rev-parse --is-inside-work-tree --show-prefix) || exit 1
if [ "$location" != true ]; then
    printf '%s\n' 'Submodule safety census did not retain its selected work tree' >&2
    exit 1
fi
# foreach does not enumerate children it cannot enter. Check this repository's
# indexed gitlinks too, including paths whose inaccessible ancestor hides .git.
scratch=$(/usr/bin/mktemp -d "$CAGENTS_PR_SUBMODULE_HELPERS/census.XXXXXX")
trap 'code=$?; trap - 0; /bin/rm -f -- "$scratch/index" "$scratch/errors" "$scratch/exit" || code=1; /bin/rmdir -- "$scratch" || code=1; exit "$code"' 0
trap 'exit 1' HUP INT TERM
(
    if "$CAGENTS_PR_GIT" --no-optional-locks -c core.fsmonitor=false \
        ls-files --stage -z 2> "$scratch/errors"; then
        printf 0 > "$scratch/exit"
    else
        printf 1 > "$scratch/exit"
    fi
) | /usr/bin/head -c 16777217 > "$scratch/index"
[ "$(/bin/cat "$scratch/exit")" = 0 ]
[ ! -s "$scratch/errors" ] || { /bin/cat "$scratch/errors" >&2; exit 1; }
[ "$(/usr/bin/wc -c < "$scratch/index")" -le 16777216 ]
if [ -s "$scratch/index" ]; then
    [ "$(/usr/bin/tail -c 1 "$scratch/index" | /usr/bin/od -An -tu1 | /usr/bin/tr -d '[:space:]')" = 0 ]
fi
/usr/bin/xargs -0 -r -s 32768 /bin/sh "$CAGENTS_PR_SUBMODULE_HELPERS/check-submodule-paths" . \
    < "$scratch/index"
case "${1-}" in
    guard) ;;
    '')
        "$CAGENTS_PR_GIT" --no-optional-locks -c core.fsmonitor=false \
            -c diff.autoRefreshIndex=false status --porcelain=v1 \
            --untracked-files=all --ignore-submodules=none -z
        ;;
    *) exit 1;;
esac
