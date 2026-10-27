#!/bin/sh
set -eu
for path do
    case "$path" in ''|/*|../*|*/../*|*/..|./*|*/./*) exit 1;; esac
    target="$CAGENTS_PR_WORKTREE_FD/$path"
    if [ -e "$target" ] || [ -L "$target" ]; then
        printf '%s\n' "Incoming PR path collides with local data: $path" >&2
        exit 1
    fi
    ancestor=$path
    while :; do
        case "$ancestor" in */*) ancestor=${ancestor%/*};; *) break;; esac
        parent="$CAGENTS_PR_WORKTREE_FD/$ancestor"
        if [ -L "$parent" ] || { [ -e "$parent" ] && { [ ! -d "$parent" ] || [ ! -r "$parent" ] || [ ! -x "$parent" ]; }; }; then
            printf '%s\n' "Incoming PR path has an unsafe or unreadable local ancestor: $ancestor" >&2
            exit 1
        fi
    done
done
