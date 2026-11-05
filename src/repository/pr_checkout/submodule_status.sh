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
exec "$CAGENTS_PR_GIT" --no-optional-locks -c core.fsmonitor=false \
    -c diff.autoRefreshIndex=false status --porcelain=v1 \
    --untracked-files=all --ignore-submodules=none -z
