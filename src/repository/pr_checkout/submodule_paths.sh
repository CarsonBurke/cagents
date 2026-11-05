#!/bin/sh
# Git may silently skip an inaccessible populated child in both status and
# submodule foreach. Check indexed gitlink paths before trusting that census.
# Records are argv, batched from ls-files --stage -z without decoding paths.
set -eu
root=$1
shift
tab=$(printf '\t')
refuse() {
    printf '%s\n' "Git could not inspect submodule; worktree was kept. Check access permissions or initialization, then retry: $path" >&2
    exit 1
}
[ -d "$root" ] && [ -r "$root" ] && [ -x "$root" ] || {
    printf '%s\n' 'Git could not inspect the selected worktree' >&2
    exit 1
}
for entry do
    case "$entry" in '160000 '*) ;; *) continue;; esac
    case "$entry" in *"$tab"*) ;; *) path='(invalid index record)'; refuse;; esac
    header=${entry%%"$tab"*}
    path=${entry#*"$tab"}
    object=${header#160000 }
    stage=${object##* }
    object=${object% *}
    case "$stage" in 0|1|2|3) ;; *) refuse;; esac
    case "$object" in ''|*[!0-9a-f]*) refuse;; esac
    case ${#object} in 40|64) ;; *) refuse;; esac
    case "$path" in ''|/*) refuse;; esac
    selected=$root
    remaining=$path
    missing=0
    while :; do
        component=${remaining%%/*}
        case "$component" in ''|.|..) refuse;; esac
        selected=$selected/$component
        # Inspect from the known-readable parent outward: an inaccessible
        # ancestor must never turn a missing descendant into a clean result.
        [ ! -L "$selected" ] || refuse
        if [ ! -e "$selected" ]; then
            missing=1
            break
        fi
        [ -d "$selected" ] && [ -r "$selected" ] && [ -x "$selected" ] || refuse
        [ "$remaining" != "$component" ] || break
        remaining=${remaining#*/}
    done
    [ "$missing" = 0 ] || continue
    marker=$selected/.git
    if [ -e "$marker" ]; then
        if [ -d "$marker" ]; then
            [ -r "$marker" ] && [ -x "$marker" ] || refuse
        else
            [ -f "$marker" ] && [ -r "$marker" ] || refuse
        fi
    elif [ -L "$marker" ]; then
        refuse
    else
        # An accessible, empty uninitialized child is safe. A nonempty omitted
        # child may contain local data even though Git does not visit it.
        contents=$(/usr/bin/find "$selected" -mindepth 1 -maxdepth 1 -printf x -quit) || refuse
        [ -z "$contents" ] || refuse
    fi
done
