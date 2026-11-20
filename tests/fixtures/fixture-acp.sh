#!/bin/sh
# The offline ACP agent installed into a native-evidence fixture as `agy` and
# as `muse-upstream`, by `native-evidence start SIDE --acp`. It answers as the
# agent it is installed as, and keeps each agent's conversations and sign-in
# under the fixture's acp/AGENT directory.
#
# Signing out is a file: `acp/AGENT/signed-out`. While it exists, `agy`
# refuses every new or loaded session as signed out, and `agy acp --login`
# removes it, as a sign-in in the browser would. Muse's sign-in commands are
# the fake agent's own.
#
# Every frame the agent receives goes to acp/AGENT/frames.log, so a check can
# read which settings and prompts cagents sent.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture=${here%/bin}
case ${0##*/} in
    muse-upstream) shape=muse ;;
    *) shape=agy ;;
esac
export FAKE_ACP_SHAPE=$shape
export FAKE_ACP_STATE="${FAKE_ACP_STATE:-$fixture/acp/$shape}"
export FAKE_ACP_LOG="${FAKE_ACP_LOG:-$FAKE_ACP_STATE/frames.log}"
# An open conversation is locked as the real agent locks it, in the
# fixture's own home only.
case ${HOME:-} in
    "$fixture"/*) export FAKE_ACP_PRESENCE=1 ;;
esac
# The turn tests/fixtures/conversation-work.py prepares, for a `work` prompt.
export FAKE_ACP_WORK="${FAKE_ACP_WORK:-$fixture/home/.claude/fixture-frames.jsonl}"
mkdir -p -- "$FAKE_ACP_STATE"
if [ "$shape" = agy ]; then
    for argument in "$@"; do
        if [ "$argument" = --login ]; then
            rm -f -- "$FAKE_ACP_STATE/signed-out"
            echo 'Signed in to Antigravity (fixture).'
            exit 0
        fi
    done
    if [ -e "$FAKE_ACP_STATE/signed-out" ] && [ -z "${FAKE_ACP_AUTH:-}" ]; then
        export FAKE_ACP_AUTH=new
    fi
fi
exec python3 "$fixture/libexec/acp/fake-acp-agent.py" "$@"
