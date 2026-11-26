# Shared by the fixture runners. The starting path is absolute and normalized.
find_git_marker_in_ancestry() {
    local ancestor=$1 marker
    while [ -n "$ancestor" ]; do
        marker=${ancestor%/}/.git
        # Git skips a sandbox's empty .git mount point. Keep treating gitfiles,
        # links and directories with a HEAD as markers, even when HEAD is a
        # dangling symbolic ref or the directory cannot be searched.
        if [ -L "$marker" ] ||
            { [ -e "$marker" ] && [ ! -d "$marker" ]; } ||
            { [ -d "$marker" ] &&
              { [ -e "$marker/HEAD" ] || [ -L "$marker/HEAD" ] || [ ! -x "$marker" ]; }; }; then
            printf '%s\n' "$marker"
            return 0
        fi
        [ "$ancestor" != / ] || break
        ancestor=${ancestor%/*}
        ancestor=${ancestor:-/}
    done
    return 1
}
