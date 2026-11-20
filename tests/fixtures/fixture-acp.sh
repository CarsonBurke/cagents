#!/bin/sh
# The offline ACP agent installed into a native-evidence fixture as `agy` and
# as `muse-upstream`, by `native-evidence start SIDE --acp`. It answers as the
# agent it is installed as, and keeps each agent's conversations and sign-in
# under the fixture's acp/AGENT directory.
#
# Signing out is a file: `acp/AGENT/signed-out`. While it exists, `agy`
# refuses every new or loaded session as signed out, and `agy acp --login`
# removes it, as a sign-in in the browser would. Muse's sign-in commands are
# the fake agent's own, and keep the fixture's config/muse/auth.json as
# Muse keeps its own: present while signed in.
#
# `acp/AGENT/no-allowance` refuses every prompt as a login with no usage
# allowance, and `acp/agy/hang-probe` makes the account probe's session
# never answer, as one that times out.
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture=${here%/bin}
case ${0##*/} in
    muse-upstream) shape=muse ;;
    *) shape=agy ;;
esac
export FAKE_ACP_SHAPE=$shape
export FAKE_ACP_STATE="${FAKE_ACP_STATE:-$fixture/acp/$shape}"
mkdir -p -- "$FAKE_ACP_STATE"
if [ "$shape" = muse ]; then
    export FAKE_ACP_AUTH_FILE="$fixture/config/muse/auth.json"
fi
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
