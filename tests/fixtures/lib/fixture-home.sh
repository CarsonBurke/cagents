# Shared POSIX shell guard for offline fixtures. Call before any writes.
# A caller may pass a fixture-provided path as the second argument.
require_fixture_home() {
    fixture_home_variable=$1
    if [ "$#" -gt 1 ]; then
        fixture_home_value=$2
    else
        if fixture_home_value=$(printenv "$fixture_home_variable" 2>/dev/null && printf '.'); then
            fixture_home_value=${fixture_home_value%.}
            fixture_home_value=${fixture_home_value%?}
        else
            fixture_home_value=
        fi
    fi
    fixture_home_resolved='<unset>'
    if [ -n "$fixture_home_value" ]; then
        if fixture_home_resolved=$(realpath -m -- "$fixture_home_value" 2>/dev/null && printf '.'); then
            fixture_home_resolved=${fixture_home_resolved%.}
            fixture_home_resolved=${fixture_home_resolved%?}
            fixture_home_ancestor=$fixture_home_resolved
            while :; do
                if [ -f "$fixture_home_ancestor/.cagents-fixture-root" ]; then
                    return 0
                fi
                [ "$fixture_home_ancestor" != / ] || break
                fixture_home_parent=${fixture_home_ancestor%/*}
                [ -n "$fixture_home_parent" ] || fixture_home_parent=/
                fixture_home_ancestor=$fixture_home_parent
            done
        else
            fixture_home_resolved=$fixture_home_value
        fi
    fi
    fixture_home_display=$(printf '%s.' "$fixture_home_resolved" |
        sed 's/\r/\\r/g' | sed ':a;N;$!ba;s/\n/\\n/g')
    fixture_home_display=${fixture_home_display%.}
    printf 'fixture-home: refusing %s=%s: not inside a marked fixture home\n' \
        "$fixture_home_variable" "$fixture_home_display" >&2
    exit 1
}
