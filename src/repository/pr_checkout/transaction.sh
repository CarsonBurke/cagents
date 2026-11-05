#!/bin/sh
set -eu
trap 'code=$?; if [ "$code" -ne 0 ]; then printf "%s\n" "PR checkout could not retain its confirmed state or repository policy refused it; refresh or inspect the retained operation" >&2; fi; exit "$code"' 0

case "$1" in preparing|prepared|committed|aborted) ;; *) exit 1;; esac
input="$CAGENTS_PR_PRIVATE/input-$1"
/bin/cat > "$input"

# Git exports its own directory and -c environment to hooks. Select the exact
# retained directories and restore caller configuration for child Git commands.
repo_git() (
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
    GIT_CONFIG_PARAMETERS=$CAGENTS_PR_ORIGINAL_CONFIG
    GIT_COMMON_DIR=$CAGENTS_PR_COMMON_FD
    export GIT_CONFIG_PARAMETERS GIT_COMMON_DIR
    exec "$CAGENTS_PR_GIT" -C "$CAGENTS_PR_WORKTREE_FD" -c core.fsmonitor=false \
        --git-dir="$CAGENTS_PR_GITDIR_FD" --work-tree="$CAGENTS_PR_WORKTREE_FD" "$@"
)
read_git() {
    repo_git --no-optional-locks -c diff.autoRefreshIndex=false "$@"
}

# Retain the exact native HEAD-lock diagnostic without changing the locale (and
# therefore user filter/hook behavior). Bound redirected evidence independently
# of the parent executor's output capture. A truncated/missing receipt refuses.
head_locked_git() {
    receipt=$1
    shift
    (
        code=0
        repo_git -c core.filesRefLockTimeout=0 "$@" 2>&1 >/dev/null || code=$?
        printf '%s' "$code" > "$CAGENTS_PR_PRIVATE/$receipt-exit"
    ) | /usr/bin/head -c 65537 > "$CAGENTS_PR_PRIVATE/$receipt-errors"
    [ "$(/bin/cat "$CAGENTS_PR_PRIVATE/$receipt-exit")" = 128 ]
    [ -s "$CAGENTS_PR_PRIVATE/$receipt-errors" ]
    [ "$(/usr/bin/wc -c < "$CAGENTS_PR_PRIVATE/$receipt-errors")" -le 65536 ]
}

verify_identity() {
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_WORKTREE")" = "$CAGENTS_PR_WORKTREE_ID" ]
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_GITDIR")" = "$CAGENTS_PR_GITDIR_ID" ]
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_COMMON")" = "$CAGENTS_PR_COMMON_ID" ]
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_WORKTREE_FD")" = "$CAGENTS_PR_WORKTREE_ID" ]
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_GITDIR_FD")" = "$CAGENTS_PR_GITDIR_ID" ]
    [ "$(/usr/bin/stat -Lc '%d:%i' -- "$CAGENTS_PR_COMMON_FD")" = "$CAGENTS_PR_COMMON_ID" ]
    [ "$(repo_git symbolic-ref --quiet HEAD)" = "$CAGENTS_PR_BRANCH" ]
    [ "$(repo_git rev-parse --verify HEAD)" = "$CAGENTS_PR_OLD" ]
    for marker in MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD rebase-merge rebase-apply sequencer MERGE_AUTOSTASH; do
        [ ! -e "$CAGENTS_PR_GITDIR_FD/$marker" ]
        [ ! -L "$CAGENTS_PR_GITDIR_FD/$marker" ]
    done
}

verify_input() {
    branch_count=0
    remote_count=0
    while IFS=' ' read -r old new ref extra; do
        [ -z "$extra" ]
        case "$ref" in
            "$CAGENTS_PR_BRANCH")
                [ "$old" = "$CAGENTS_PR_OLD" ]
                [ "$new" = "$CAGENTS_PR_NEW" ]
                branch_count=$((branch_count + 1))
                ;;
            "$CAGENTS_PR_REMOTE_REF")
                [ "$CAGENTS_PR_UPDATE_REMOTE" = 1 ]
                [ "$old" = "$CAGENTS_PR_REMOTE_OLD" ]
                [ "$new" = "$CAGENTS_PR_NEW" ]
                remote_count=$((remote_count + 1))
                ;;
            *) exit 1;;
        esac
    done < "$input"
    [ "$branch_count" = 1 ]
    [ "$remote_count" = "$CAGENTS_PR_UPDATE_REMOTE" ]
}

verify_config() {
    read_git config --null --list --show-scope > "$CAGENTS_PR_PRIVATE/config-current"
    /usr/bin/cmp -s "$CAGENTS_PR_PRIVATE/config-expected" "$CAGENTS_PR_PRIVATE/config-current"
}

verify_upstream() {
    [ "$(read_git for-each-ref '--format=%(upstream)' "$CAGENTS_PR_BRANCH")" = "$CAGENTS_PR_EXPECTED_UPSTREAM" ]
}

previous_hook() {
    if [ -n "$CAGENTS_PR_PREVIOUS_HOOK" ] && [ -x "$CAGENTS_PR_PREVIOUS_HOOK" ]; then
        if ! GIT_CONFIG_PARAMETERS="$CAGENTS_PR_PREVIOUS_CONFIG" "$CAGENTS_PR_PREVIOUS_HOOK" "$@" < "$input"; then
            printf '%s\n' "$1" > "$CAGENTS_PR_PRIVATE/policy-failed"
            return 1
        fi
    fi
}

if [ "$1" = preparing ]; then
    # Git 2.56 reports this before acquiring reference locks and resolving
    # symbolic HEAD. It cannot authorize checkout or supply prepared proof.
    # Capability probes remain policy-free; real transactions retain the
    # original hook's opportunity to refuse this earlier phase.
    if [ "$CAGENTS_PR_MODE" != probe ]; then
        previous_hook "$@"
    fi
    exit 0
fi

if [ "$CAGENTS_PR_MODE" = probe ]; then
    if [ "$1" = prepared ]; then
        verify_input
        verify_identity
        verify_config
        verify_upstream
        printf prepared > "$CAGENTS_PR_PRIVATE/probe"
    fi
    exit 0
fi

if [ "$1" = prepared ]; then
    # A different symbolic HEAD can share the same OID. Bind its actual
    # transaction before invoking policy and again before any checkout effect.
    verify_input
    verify_identity
    verify_config
    verify_upstream
    previous_hook "$@"
    verify_input
    verify_identity
    verify_config
    verify_upstream
    if [ "$CAGENTS_PR_UPDATE_REMOTE" = 1 ]; then
        # An unexpected symbolic remote ref must not be replaced by an OID.
        if repo_git symbolic-ref --quiet "$CAGENTS_PR_REMOTE_REF"; then exit 1; else [ "$?" = 1 ]; fi
    fi
    if [ "$CAGENTS_PR_OLD" != "$CAGENTS_PR_NEW" ]; then
        (
            if {
                read_git status --porcelain=v2 --untracked-files=all --ignore-submodules=none -z &&
                read_git submodule foreach --quiet --recursive \
                    '/bin/sh "$CAGENTS_PR_PRIVATE/submodule-status"'
            } 2> "$CAGENTS_PR_PRIVATE/status-errors"; then
                printf 0 > "$CAGENTS_PR_PRIVATE/status-exit"
            else
                printf 1 > "$CAGENTS_PR_PRIVATE/status-exit"
            fi
        ) | /usr/bin/head -c 1 > "$CAGENTS_PR_PRIVATE/status"
        [ ! -s "$CAGENTS_PR_PRIVATE/status" ]
        [ "$(/bin/cat "$CAGENTS_PR_PRIVATE/status-exit")" = 0 ]
        [ ! -s "$CAGENTS_PR_PRIVATE/status-errors" ]
        # Diagnose pre-existing incoming collisions, including directory/file
        # collisions and symlink ancestors, before starting native checkout.
        # This census is NOT authority to overwrite a later ignored file.
        # Unrelated ignored build directories remain intact.
        # Bound both captured bytes and each xargs batch; preserve NUL paths.
        (
            if read_git diff-tree --no-commit-id --name-only --diff-filter=A --no-renames -r -z \
                "$CAGENTS_PR_OLD" "$CAGENTS_PR_NEW" 2> "$CAGENTS_PR_PRIVATE/additions-errors"; then
                printf 0 > "$CAGENTS_PR_PRIVATE/additions-exit"
            else
                printf 1 > "$CAGENTS_PR_PRIVATE/additions-exit"
            fi
        ) | /usr/bin/head -c 16777217 > "$CAGENTS_PR_PRIVATE/additions"
        [ "$(/bin/cat "$CAGENTS_PR_PRIVATE/additions-exit")" = 0 ]
        [ ! -s "$CAGENTS_PR_PRIVATE/additions-errors" ]
        [ "$(/usr/bin/wc -c < "$CAGENTS_PR_PRIVATE/additions")" -le 16777216 ]
        if [ -s "$CAGENTS_PR_PRIVATE/additions" ]; then
            [ "$(/usr/bin/tail -c 1 "$CAGENTS_PR_PRIVATE/additions" | /usr/bin/od -An -tu1 | /usr/bin/tr -d '[:space:]')" = 0 ]
        fi
        /usr/bin/xargs -0 -r -s 32768 /bin/sh "$CAGENTS_PR_PRIVATE/check-new-paths" \
            < "$CAGENTS_PR_PRIVATE/additions"
        printf started > "$CAGENTS_PR_PRIVATE/checkout-started"
        # The outer native reference transaction retains HEAD.lock AND the
        # branch lease throughout this block. Check the lock's native refusal
        # before invoking the porcelain two-tree checkout. The probe is a
        # nominal update, so its unexpected success is an uncertain effect;
        # checkout-started was deliberately recorded before it.
        [ -f "$CAGENTS_PR_GITDIR_FD/HEAD.lock" ]
        [ ! -L "$CAGENTS_PR_GITDIR_FD/HEAD.lock" ]
        head_locked_git head-lock-probe update-ref --no-deref HEAD "$CAGENTS_PR_OLD"
        verify_identity
        verify_config
        verify_upstream

        # Unlike read-tree -m -u, this native unpack preserves ignored entries
        # at its own checkout check. Native checkout writes the index/worktree
        # before trying to detach HEAD; the parent's existing HEAD.lock prevents
        # that final publication. Only the identical native lock refusal is
        # admissible here, never a generic failing checkout or a successful
        # detach. User post-index-change policy stays native/original; the failed
        # HEAD update does not reach post-checkout.
        head_locked_git checkout checkout --no-overwrite-ignore --detach --no-progress --quiet "$CAGENTS_PR_NEW"
        /usr/bin/cmp -s "$CAGENTS_PR_PRIVATE/head-lock-probe-errors" "$CAGENTS_PR_PRIVATE/checkout-errors"
        [ -f "$CAGENTS_PR_GITDIR_FD/HEAD.lock" ]
        [ ! -L "$CAGENTS_PR_GITDIR_FD/HEAD.lock" ]
        # Compare the complete staged tree, not status against still-old HEAD.
        # These two read-only comparisons prohibit a carried staged change or
        # failed/partial writeout. No repair/reset/replay follows a refusal.
        read_git diff-index --cached --quiet --no-ext-diff --no-textconv --ignore-submodules=none "$CAGENTS_PR_NEW" --
        read_git diff-files --quiet --no-ext-diff --no-textconv --ignore-submodules=none --
        verify_input
        verify_identity
        verify_config
        verify_upstream
        printf completed > "$CAGENTS_PR_PRIVATE/checkout-completed"
    fi
    printf prepared > "$CAGENTS_PR_PRIVATE/effect-prepared"
elif [ "$1" = committed ]; then
    printf committed > "$CAGENTS_PR_PRIVATE/ref-committed"
    previous_hook "$@"
else
    previous_hook "$@"
fi
