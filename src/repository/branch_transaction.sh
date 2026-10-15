#!/bin/sh
set -eu
input="$CAGENTS_BRANCH_PRIVATE/input-$1"
case "$1" in prepared|committed|aborted) ;; *) exit 1;; esac
/bin/cat > "$input"
repo_git() (
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
    GIT_CONFIG_PARAMETERS=$CAGENTS_BRANCH_ORIGINAL_CONFIG
    GIT_COMMON_DIR=$CAGENTS_BRANCH_FD_2
    export GIT_CONFIG_PARAMETERS GIT_COMMON_DIR
    exec "$CAGENTS_BRANCH_GIT" -C "$CAGENTS_BRANCH_FD_0" -c core.fsmonitor=false \
        --git-dir="$CAGENTS_BRANCH_FD_1" --work-tree="$CAGENTS_BRANCH_FD_0" "$@"
)
verify() {
    . "$CAGENTS_BRANCH_PRIVATE/check-identities"
    count=0
    while IFS=' ' read -r old new ref extra; do
        [ -z "$extra" ]
        [ "$ref" = "$CAGENTS_BRANCH_REF" ]
        [ "$new" = "$CAGENTS_BRANCH_NEW" ]
        # Native branch --force can omit an expected-old argument. Bind the
        # ACTUAL current OID while Git holds its prepared lock in either case.
        case "$old" in *[!0]*) [ "$old" = "$CAGENTS_BRANCH_OLD" ];; esac
        count=$((count + 1))
    done < "$input"
    [ "$count" = 1 ]
    [ "$(repo_git rev-parse --verify --end-of-options "$CAGENTS_BRANCH_REF")" = "$CAGENTS_BRANCH_OLD" ]
    if repo_git symbolic-ref --quiet "$CAGENTS_BRANCH_REF"; then exit 1; else [ "$?" = 1 ]; fi
    repo_git merge-base --is-ancestor "$CAGENTS_BRANCH_OLD" "$CAGENTS_BRANCH_NEW"
    repo_git --no-optional-locks -c diff.autoRefreshIndex=false config --null --list --show-scope \
        > "$CAGENTS_BRANCH_PRIVATE/config-current"
    /usr/bin/cmp -s "$CAGENTS_BRANCH_PRIVATE/config-expected" "$CAGENTS_BRANCH_PRIVATE/config-current"
}
previous() {
    if [ -n "$CAGENTS_BRANCH_PREVIOUS_HOOK" ] && [ -x "$CAGENTS_BRANCH_PREVIOUS_HOOK" ]; then
        GIT_CONFIG_PARAMETERS="$CAGENTS_BRANCH_PREVIOUS_CONFIG" "$CAGENTS_BRANCH_PREVIOUS_HOOK" "$@" < "$input"
    fi
}
if [ "$1" = prepared ]; then
    verify
    previous "$@"
    verify
    printf prepared > "$CAGENTS_BRANCH_PROOF"
else
    previous "$@"
fi
