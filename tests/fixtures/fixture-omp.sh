#!/bin/sh
# The offline Oh My Pi peer installed into a native-evidence fixture as `omp`,
# by `native-evidence start SIDE --omp`. See tests/fixtures/omp-child-errors.py
# for the conversation it replays. A session started without --omp has no
# `omp` on PATH, so the app sees Oh My Pi as not installed.
set -eu
. "$(dirname -- "$(readlink -f -- "$0")")/lib/fixture-home.sh"
require_fixture_home HOME
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture=${here%/bin}
exec python3 -I "$fixture/libexec/omp/omp-child-errors.py" "$@"
