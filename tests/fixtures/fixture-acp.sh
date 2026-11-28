#!/bin/sh
# The offline ACP agent installed into a native-evidence fixture as `agy` and
# as `muse-upstream`, by `native-evidence start SIDE --acp`. It answers as the
# agent it is installed as, and keeps each agent's conversations and sign-in
# under the fixture's acp/AGENT directory.
#
# Signing out is a file: `acp/AGENT/signed-out`. While it exists, `agy`
# refuses every new or loaded session as signed out, and `agy acp --login`
# removes it, as a sign-in in the browser would. Muse's sign-in commands are
# the fake agent's own, and keep the fixture's Muse sign-in record where Muse
# keeps its own: present while signed in.
#
# `acp/agy/keyring-locked` keeps Antigravity's sign-in in a locked desktop
# keyring, out of reach of an `agy` that does not keep it in a file, and
# `acp/agy/version` names the version it reports. `acp/agy/keyring-only`
# keeps it in the keyring alone, as before cagents kept it in a file, so an
# `agy` that keeps it in a file is signed out until `agy acp --login` signs
# it in there.
#
# `acp/AGENT/no-allowance` refuses every prompt as a login with no usage
# allowance, and `acp/agy/hang-probe` makes the account probe's session
# never answer, as one that times out. `acp/AGENT/background-seconds` holds
# how long a `background` prompt's command runs, and `acp/AGENT/cancel-seconds`
# how long a stopped turn takes to end.
#
# `acp/muse/initial-model` sets the model Muse reports when opening a session.
#
# Every frame the agent receives goes to acp/AGENT/frames.log, so a check can
# read which settings and prompts cagents sent.
set -eu
. "$(dirname -- "$(readlink -f -- "$0")")/lib/fixture-home.sh"
require_fixture_home HOME
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
require_fixture_home FAKE_ACP_STATE
require_fixture_home FAKE_ACP_LOG
mkdir -p -- "$FAKE_ACP_STATE"
if [ -z "${FAKE_ACP_BACKGROUND_SECONDS:-}" ] && [ -s "$FAKE_ACP_STATE/background-seconds" ]; then
    FAKE_ACP_BACKGROUND_SECONDS=$(cat -- "$FAKE_ACP_STATE/background-seconds")
    export FAKE_ACP_BACKGROUND_SECONDS
fi
if [ -z "${FAKE_ACP_CANCEL_SECONDS:-}" ] && [ -s "$FAKE_ACP_STATE/cancel-seconds" ]; then
    FAKE_ACP_CANCEL_SECONDS=$(cat -- "$FAKE_ACP_STATE/cancel-seconds")
    export FAKE_ACP_CANCEL_SECONDS
fi
if [ "$shape" = muse ]; then
    export FAKE_ACP_AUTH_FILE="$fixture/config/muse/auth.json"
fi
if [ "$shape" = agy ]; then
    for argument in "$@"; do
        if [ "$argument" = --login ]; then
            rm -f -- "$FAKE_ACP_STATE/signed-out"
            if [ -n "${AGY_CLI_FORCE_FILE_TOKEN_STORAGE:-}" ]; then
                rm -f -- "$FAKE_ACP_STATE/keyring-only"
            fi
            echo 'Signed in to Antigravity (fixture).'
            exit 0
        fi
    done
    if [ -e "$FAKE_ACP_STATE/signed-out" ] && [ -z "${FAKE_ACP_AUTH:-}" ]; then
        export FAKE_ACP_AUTH=new
    fi
fi
exec python3 "$fixture/libexec/acp/fake-acp-agent.py" "$@"
