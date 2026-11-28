#!/bin/sh
# Install, upgrade or remove cagents for the current user on Apple Silicon
# macOS. See docs/releasing.md#macos for the layout.
set -eu

# The coordinator opens an ordinary script file and gives it the publication
# fence. Spool a private copy so the same file is available to curl | sh.
installer_dir=$(mktemp -d "${TMPDIR:-/tmp}/cagents-installer.XXXXXX")
trap 'rm -rf "$installer_dir"' EXIT
trap 'exit 1' HUP INT TERM
cat > "$installer_dir/install.sh" <<'CAGENTS_INSTALLER'
set -eu

PROGRAM=cagents-install
DEFAULT_REPOSITORY='<OWNER>/<REPO>'
TARGET=aarch64-apple-darwin
LABEL=dev.cagents.Cagents.Daemon
MARKER_TEXT='cagents install root'
PROGRAMS='cagents cagentsd cagents-ui cagents-dictate'
APP_LAUNCHER=Contents/MacOS/Cagents
LOGIN_COMMAND='CDPATH= cd -P "$1/current/bin" && exec "$PWD/cagents" daemon start'
LOGIN_NAME=cagents-login

usage() {
    cat <<'EOF'
Usage: install-macos.sh [OPTIONS]

Install the cagents Apple Silicon macOS release for the current user.

  ~/Library/Application Support/cagents-install   versions, and Cagents.app
  ~/Applications/Cagents.app                      a link to Cagents.app
  ~/.local/bin/cagents                            the command line
  ~/Library/LaunchAgents/dev.cagents.Cagents.Daemon.plist
                                                  starts the daemon at login

Quit cagents and stop its daemon before replacing or removing an installation.

Options:
  --version VERSION  Install a release version (default: the latest release)
  --prefix PATH      Installation root (default: ~/Library/Application Support/cagents-install)
  --no-start         Install the login item without starting the daemon now
  --uninstall        Remove the program, keeping your cagents data
  --purge            Uninstall and also remove your cagents data
  --help             Show this help

Environment:
  CAGENTS_REPOSITORY  GitHub OWNER/REPO used for downloads
  CAGENTS_ARCHIVE     Local release archive (requires or infers --version)
  CAGENTS_CHECKSUMS   Local SHA256SUMS-macos manifest
EOF
}

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

note() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
}

VERSION=
PREFIX=
START=1
UNINSTALL=0
PURGE=0
PUBLISH_TREE=
PUBLISHER=
PUBLISH_REMOVAL=0
EXTRACT_DIR=
while [ "$#" -gt 0 ]; do
    case $1 in
        --version|--prefix|--publish-tree|--publisher|--extract-update)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"
            case $1 in
                --version) VERSION=${2#v} ;;
                --prefix) PREFIX=$2 ;;
                --publish-tree) PUBLISH_TREE=$2 ;;
                --publisher) PUBLISHER=$2 ;;
                --extract-update) EXTRACT_DIR=$2 ;;
            esac
            shift 2
            ;;
        --no-start) START=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) UNINSTALL=1; PURGE=1; shift ;;
        --publish-removal) PUBLISH_REMOVAL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1 (see --help)" ;;
    esac
done

if [ "${CAGENTS_AGENT_AUTHORITY+x}" = x ]; then
    fail 'installation and removal require your own terminal; hosted sessions cannot change the installation. Cagents terminals are hosted sessions. A person must run this command from a system terminal outside cagents.'
fi
[ "$(id -u)" -ne 0 ] || fail 'refusing to run as root; install cagents from your user account'
[ "$(uname -s)" = Darwin ] || fail 'this installer is for macOS; on Linux use install.sh'
[ "$(uname -m)" = arm64 ] || fail 'only Apple Silicon Macs are supported'
: "${HOME:?HOME is not set}"
case $HOME in
    /*) ;;
    *) fail 'HOME must be an absolute path' ;;
esac
for command in awk cmp codesign curl diff ditto find launchctl ln mkdir mktemp mv osascript plutil rm shasum stat tar; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

if [ -n "$EXTRACT_DIR" ]; then
    [ "$UNINSTALL" -eq 0 ] && [ "$PUBLISH_REMOVAL" -eq 0 ] \
        && [ -z "$PUBLISH_TREE" ] && [ -z "$PUBLISHER" ] \
        || fail 'an update extraction cannot publish or remove an installation'
    [ -n "$PREFIX" ] && [ -n "$VERSION" ] || fail 'update extraction needs --prefix and --version'
    case ${CAGENTS_ARCHIVE:-} in
        /*) ;;
        *) fail 'update extraction needs an absolute CAGENTS_ARCHIVE file path' ;;
    esac
    case ${CAGENTS_CHECKSUMS:-} in
        /*) ;;
        *) fail 'update extraction needs an absolute CAGENTS_CHECKSUMS file path' ;;
    esac
    [ -f "$CAGENTS_ARCHIVE" ] && [ -f "$CAGENTS_CHECKSUMS" ] \
        || fail 'update extraction needs the local archive and checksum files'
fi

PREFIX=${PREFIX:-"$HOME/Library/Application Support/cagents-install"}
case $PREFIX in
    /*) ;;
    *) fail '--prefix must be an absolute path' ;;
esac
PREFIX=${PREFIX%/}
MARKER=$PREFIX/.cagents-install-root
PENDING_MARKER=$MARKER.pending
APP=$PREFIX/Cagents.app
APP_LINK=$HOME/Applications/Cagents.app
BIN_LINK=$HOME/.local/bin/cagents
AGENT_PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
DOMAIN=gui/$(id -u)
PLIST_BUDDY=/usr/libexec/PlistBuddy
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
umask 022

# Launch Services is per account, not per HOME: only an installation in the
# account's own home registers Cagents.app, so a private test install (as
# scripts/package-macos makes) leaves the account's registrations alone.
ACCOUNT_HOME=$(dscl . -read "/Users/$(id -un)" NFSHomeDirectory 2>/dev/null | sed -n 's/^NFSHomeDirectory: //p') \
    || ACCOUNT_HOME=
# Whether HOME is the account's own home, compared by real path. An account
# whose home the local directory does not name counts as private.
own_home() {
    [ -n "$ACCOUNT_HOME" ] && [ "$(cd "$HOME" 2>/dev/null && pwd -P)" = "$(cd "$ACCOUNT_HOME" 2>/dev/null && pwd -P)" ]
}
register_app() {
    own_home && [ -x "$LSREGISTER" ] || return 0
    "$LSREGISTER" "$@" >/dev/null 2>&1 || :
}

# The installation this script made, or nothing at PREFIX at all.
owned_prefix() {
    [ -f "$MARKER" ] && [ ! -L "$MARKER" ] && [ "$(cat "$MARKER")" = "$MARKER_TEXT" ]
}

# The update worker owns this directory for the lifetime of the extraction.
# Work inside it so publishing the validated tree needs only a local rename.
verify_extract_dir() {
    uid=$(id -u)
    [ "$PREFIX" = "$(CDPATH= cd -P "$PREFIX" && pwd -P)" ] && owned_prefix \
        && [ "$(stat -f %u "$PREFIX")" = "$uid" ] \
        || fail 'update extraction needs an owned, physical installation prefix'
    prefix_mode=$(stat -f %Lp "$PREFIX") || fail 'cannot inspect the installation permissions'
    [ "$((0$prefix_mode & 022))" -eq 0 ] || fail 'the installation directory is writable by others'
    [ "${EXTRACT_DIR%/*}" = "$PREFIX/.update" ] \
        && [ "$EXTRACT_DIR" = "$(CDPATH= cd -P "$EXTRACT_DIR" && pwd -P)" ] \
        || fail 'update extraction needs a physical directory directly under .update'
    [ "$(stat -f '%u:%Lp' "$PREFIX/.update")" = "$uid:700" ] \
        && [ "$(stat -f '%u:%Lp' "$EXTRACT_DIR")" = "$uid:700" ] \
        || fail 'the update extraction directories must be private'
    entries=$(find "$EXTRACT_DIR" -mindepth 1 -maxdepth 1 -print) \
        || fail 'cannot inspect the update extraction directory'
    [ -z "$entries" ] || fail 'the update extraction directory must be empty'
}

verify_program_versions() {
    for program in $PROGRAMS; do
        reported=$("$1/bin/$program" --version) || fail "cagents $VERSION could not run $program on this Mac"
        [ "$reported" = "$program $VERSION" ] \
            || fail "the release holds $reported, expected $program $VERSION"
    done
}

release_executable() {
    [ -f "$1" ] && [ ! -L "$1" ] && [ -x "$1" ]
}

same_release_tree() {
    [ -d "$2" ] && [ ! -L "$2" ] && [ ! -L "$2/bin" ] || return 1
    for program in $PROGRAMS; do
        release_executable "$2/bin/$program" || return 1
    done
    release_executable "$2/Cagents.app/$APP_LAUNCHER" && diff -rq "$1" "$2" >/dev/null 2>&1
}

# Removes LINK when it is a symbolic link to TARGET, and only then.
remove_owned_link() {
    if [ -L "$1" ] && [ "$(readlink "$1")" = "$2" ]; then
        rm -f "$1"
    fi
}

# The daemon of this user's production instance, as the CLI at $1 sees it:
# prints its version (or "unknown"), or nothing when none runs. Fails when it
# cannot tell, including when the status is not what this script expects.
# JavaScript for Automation reads the JSON on every macOS; plutil cannot hold
# its nulls.
running_daemon() {
    status=$("$1" --no-start daemon status --json 2>/dev/null) || return 1
    /usr/bin/osascript -l JavaScript -e '
function run(argv) {
    const data = JSON.parse(argv[0]).data;
    if (typeof data.running !== "boolean" || typeof data.daemon_present !== "boolean")
        throw new Error("unexpected daemon status");
    if (!data.running && !data.daemon_present) return "";
    return typeof data.daemon_version === "string" && data.daemon_version !== "" ? data.daemon_version : "unknown";
}' "$status" 2>/dev/null
}

# The daemon status as the installed CLI reads it, which speaks the running
# daemon's protocol; the CLI at $1 when nothing is installed.
installed_daemon() {
    if [ -x "$PREFIX/current/bin/cagents" ]; then
        running_daemon "$PREFIX/current/bin/cagents"
    else
        running_daemon "$1"
    fi
}

require_publication_fence() {
    "$1" --no-start verify-install-guard --publish-only >/dev/null \
        || fail 'the installer could not verify publication ownership; no installation files were changed'
}

can_coordinate_publication() {
    "$1" coordinate-install --help 2>/dev/null | grep -q -- '--publish-only' \
        && "$1" verify-install-guard --help 2>/dev/null | grep -q -- '--publish-only'
}

# Recognize this installation's stable login item and its older versioned form.
owned_agent() {
    [ -f "$AGENT_PLIST" ] || return 1
    program=$("$PLIST_BUDDY" -c 'Print :ProgramArguments:0' "$AGENT_PLIST" 2>/dev/null) || return 1
    case $program in
        "$PREFIX/versions/"*) return 0 ;;
        /bin/sh)
            [ "$("$PLIST_BUDDY" -c 'Print :ProgramArguments:1' "$AGENT_PLIST")" = -c ] \
                && [ "$("$PLIST_BUDDY" -c 'Print :ProgramArguments:2' "$AGENT_PLIST")" = "$LOGIN_COMMAND" ] \
                && [ "$("$PLIST_BUDDY" -c 'Print :ProgramArguments:3' "$AGENT_PLIST")" = "$LOGIN_NAME" ] \
                && [ "$("$PLIST_BUDDY" -c 'Print :ProgramArguments:4' "$AGENT_PLIST")" = "$PREFIX" ]
            ;;
        *) return 1 ;;
    esac
}

stop_instructions() {
    printf '%s\n' \
        "To install over it safely: save your work, finish or close every hosted session, and quit every cagents window." \
        "Then run \`cagents --no-start daemon stop --require-empty\` (it refuses while sessions remain), check that" \
        "\`cagents --no-start daemon status\` reports stopped, and run this installer again."
}

# Unloads the login item, only when the job launchd has loaded is this
# HOME's: a private test install must not unload the account's own.
unload_agent() {
    loaded=$(launchctl print "$DOMAIN/$LABEL" 2>/dev/null | awk '$1 == "path" && $2 == "=" { sub(/^[^=]*= /, ""); print; exit }') \
        || return 0
    [ "$loaded" = "$AGENT_PLIST" ] || return 0
    launchctl bootout "$DOMAIN/$LABEL" >/dev/null 2>&1 || :
}

if [ "$UNINSTALL" -eq 1 ]; then
    [ -z "$PUBLISH_TREE" ] && [ -z "$PUBLISHER" ] || fail 'invalid removal request'
    if [ -e "$PREFIX" ] || [ -L "$PREFIX" ]; then
        [ -d "$PREFIX" ] && [ ! -L "$PREFIX" ] || fail "$PREFIX is not an installation directory; nothing was removed"
        owned_prefix || fail "$PREFIX was not made by this installer; nothing was removed"
        current_cli=$PREFIX/current/bin/cagents
        [ -x "$current_cli" ] || fail "$PREFIX has no installed cagents to coordinate removal; repair the installation first"
        if [ "$PUBLISH_REMOVAL" -eq 0 ]; then
            can_coordinate_publication "$current_cli" \
                || fail 'this version needs its own release installer for removal'
            set -- --prefix "$PREFIX" --uninstall --publish-removal
            [ "$PURGE" -eq 0 ] || set -- "$@" --purge
            exec "$current_cli" --no-start coordinate-install --prefix "$PREFIX" --installer "$0" --publish-only -- "$@"
        fi
        require_publication_fence "$current_cli"
        daemon=$(running_daemon "$current_cli") \
            || fail 'cannot tell whether the cagents daemon is running; nothing was removed'
        if [ -n "$daemon" ]; then
            printf '%s: the cagents daemon (%s) is running; nothing was removed.\n' "$PROGRAM" "$daemon" >&2
            stop_instructions >&2
            exit 1
        fi
    elif [ "$PURGE" -eq 1 ]; then
        fail "no cagents is installed at $PREFIX to check for a running daemon; stop it with \`cagents daemon stop\`, then remove ~/Library/Application Support/dev.cagents.cagents yourself"
    else
        printf 'No cagents installation at %s.\n' "$PREFIX"
        exit 0
    fi
    # Once removal starts, finish it under the inherited fence. Interrupting
    # after the selected CLI is removed would strand a partially removed install.
    trap '' HUP INT TERM PIPE
    if owned_agent; then
        unload_agent
        rm -f "$AGENT_PLIST"
    fi
    remove_owned_link "$BIN_LINK" "$PREFIX/current/bin/cagents"
    remove_owned_link "$APP_LINK" "$APP"
    [ ! -d "$APP" ] || register_app -u "$APP"
    rm -rf "$APP" "$PREFIX/.update" "$PREFIX"/.stage.* "$PREFIX"/.current.* "$PREFIX/.previous"
    if [ "$PURGE" -eq 1 ]; then
        # ProjectDirs("dev", "cagents", "cagents") on macOS.
        rm -rf "$HOME/Library/Application Support/dev.cagents.cagents" \
            "$HOME/Library/Caches/dev.cagents.cagents" \
            "$HOME/Library/Preferences/dev.cagents.cagents"
        runtime=$(/usr/bin/getconf DARWIN_USER_DIR 2>/dev/null || :)
        case ${XDG_RUNTIME_DIR:-} in
            /*)
                # With this alias, cagents/ also holds the default runtime
                # and its admission leases. Leave their subtree intact.
                if [ -n "$runtime" ] && [ ! "$XDG_RUNTIME_DIR" -ef "$runtime" ]; then
                    rm -rf "${XDG_RUNTIME_DIR%/}/cagents"
                fi
                ;;
            *) [ -z "$runtime" ] || rm -rf "${runtime%/}/cagents/cagents" ;;
        esac
    fi
    rm -rf "$PREFIX/versions" "$PREFIX/current"
    rm -f "$MARKER"
    rmdir "$PREFIX" 2>/dev/null || note "kept $PREFIX, which holds files this installer did not make"
    if [ "$PURGE" -eq 1 ]; then
        printf 'Removed cagents and its data.\n'
    else
        printf 'Removed cagents and kept your data in ~/Library/Application Support/dev.cagents.cagents.\n'
    fi
    exit 0
fi
[ "$PUBLISH_REMOVAL" -eq 0 ] || fail 'invalid publication request'

if [ -z "$PUBLISH_TREE" ]; then
    if [ -n "$EXTRACT_DIR" ]; then
        verify_extract_dir
        STAGE=$(mktemp -d "$EXTRACT_DIR/.unpacking.XXXXXX")
    else
        STAGE=$(mktemp -d "${TMPDIR:-/tmp}/cagents-download.XXXXXX")
    fi
    trap 'rm -rf "$STAGE"' EXIT
    trap 'exit 1' HUP INT TERM

    if [ -z "$EXTRACT_DIR" ]; then
        REPOSITORY=${CAGENTS_REPOSITORY:-$DEFAULT_REPOSITORY}
        case $REPOSITORY in
            '<OWNER>/<REPO>'|'') fail 'set CAGENTS_REPOSITORY to the GitHub OWNER/REPO for this project' ;;
            */*/*|*[!A-Za-z0-9._/-]*) fail "invalid repository: $REPOSITORY" ;;
        esac
    fi
    CURL_OPTIONS='--disable --proto =https --tlsv1.2 --fail --location --silent --show-error'
    LOCAL_ARCHIVE=${CAGENTS_ARCHIVE:-}
    LOCAL_CHECKSUMS=${CAGENTS_CHECKSUMS:-}
    if [ -n "$LOCAL_ARCHIVE" ] && [ -z "$VERSION" ]; then
        name=${LOCAL_ARCHIVE##*/}
        case $name in
            cagents-*-"$TARGET".tar.gz) VERSION=${name#cagents-}; VERSION=${VERSION%-"$TARGET".tar.gz} ;;
            *) fail 'cannot infer the version from CAGENTS_ARCHIVE; pass --version' ;;
        esac
    fi
    if [ -z "$VERSION" ]; then
        latest_url=$(curl $CURL_OPTIONS -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest") \
            || fail 'could not resolve the latest release'
        case $latest_url in
            https://github.com/"$REPOSITORY"/releases/tag/v*) VERSION=${latest_url##*/v} ;;
            *) fail 'the latest release did not redirect to a v* tag' ;;
        esac
    fi
    case $VERSION in
        ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "invalid version: $VERSION" ;;
    esac
    TOP_LEVEL=cagents-$VERSION
    ARCHIVE_NAME=cagents-$VERSION-$TARGET.tar.gz

    download() {
        curl $CURL_OPTIONS -o "$STAGE/$1" "https://github.com/$REPOSITORY/releases/download/v$VERSION/$1" \
            || fail "could not download $1 for cagents $VERSION"
    }
    if [ -n "$LOCAL_ARCHIVE" ]; then cp "$LOCAL_ARCHIVE" "$STAGE/$ARCHIVE_NAME"; else download "$ARCHIVE_NAME"; fi
    if [ -n "$LOCAL_CHECKSUMS" ]; then cp "$LOCAL_CHECKSUMS" "$STAGE/SHA256SUMS-macos"; else download SHA256SUMS-macos; fi
    expected=$(awk -v target="$ARCHIVE_NAME" '
        NF == 0 { next }
        NF != 2 || length($1) != 64 || $1 !~ /^[0-9a-fA-F]+$/ { bad = 1; next }
        $2 == target { matches++; hash = tolower($1) }
        END { if (bad || matches != 1) exit 1; print hash }
    ' "$STAGE/SHA256SUMS-macos") || fail 'SHA256SUMS-macos is malformed or lacks exactly one archive checksum'
    actual=$(shasum -a 256 "$STAGE/$ARCHIVE_NAME" | awk '{ print tolower($1) }')
    [ "$actual" = "$expected" ] || fail "$ARCHIVE_NAME does not match its checksum"

    # Accept exactly the release's members, so the archive cannot place files
    # anywhere else or smuggle links into the version tree.
    tar -tzf "$STAGE/$ARCHIVE_NAME" > "$STAGE/members" || fail "cannot read $ARCHIVE_NAME"
    for member in \
        '' bin/ bin/cagents bin/cagents-dictate bin/cagents-ui bin/cagentsd \
        Cagents.app/ Cagents.app/Contents/ Cagents.app/Contents/Info.plist \
        Cagents.app/Contents/MacOS/ Cagents.app/Contents/MacOS/Cagents Cagents.app/Contents/PkgInfo \
        Cagents.app/Contents/Resources/ Cagents.app/Contents/Resources/Cagents.icns \
        Cagents.app/Contents/_CodeSignature/ Cagents.app/Contents/_CodeSignature/CodeResources \
        share/ share/doc/ share/doc/cagents/ share/doc/cagents/LICENSE share/doc/cagents/THIRD-PARTY-NOTICES
    do
        printf '%s\n' "$TOP_LEVEL/$member"
    done > "$STAGE/expected"
    sed 's|/$||' "$STAGE/expected" | LC_ALL=C sort > "$STAGE/expected-paths"
    sed 's|/$||' "$STAGE/members" | LC_ALL=C sort | cmp -s - "$STAGE/expected-paths" \
        || fail "$ARCHIVE_NAME has missing or unexpected members"
    mkdir "$STAGE/tree"
    tar -xzf "$STAGE/$ARCHIVE_NAME" -C "$STAGE/tree" --no-same-owner || fail "cannot unpack $ARCHIVE_NAME"
    NEW=$STAGE/tree/$TOP_LEVEL
    while IFS= read -r member; do
        path=$STAGE/tree/${member%/}
        [ ! -L "$path" ] || fail "$ARCHIVE_NAME holds a symbolic link at $member"
        case $member in
            */) [ -d "$path" ] || fail "$ARCHIVE_NAME needs a directory at $member" ;;
            *) [ -f "$path" ] && [ "$(stat -f %l "$path")" = 1 ] \
                || fail "$ARCHIVE_NAME needs a regular, unlinked file at $member" ;;
        esac
    done < "$STAGE/expected"
    # A download through a browser carries quarantine, which Gatekeeper would
    # turn into a refusal for an ad hoc signed app the person chose to install.
    xattr -dr com.apple.quarantine "$NEW" 2>/dev/null || :
    for path in "$NEW/bin/cagents" "$NEW/bin/cagentsd" "$NEW/bin/cagents-ui" "$NEW/bin/cagents-dictate" "$NEW/Cagents.app"; do
        codesign --verify --strict "$path" 2>/dev/null || fail "${path#"$NEW/"} is not intact; download the release again"
    done
    verify_program_versions "$NEW"

    if [ -n "$EXTRACT_DIR" ]; then
        mv "$NEW" "$EXTRACT_DIR/$TOP_LEVEL" || fail 'cannot keep the verified update tree'
        exit 0
    fi

    NEW=$(cd "$NEW" && pwd -P) || fail 'cannot locate the verified release tree'
    publisher=$NEW/bin/cagents
    if ! can_coordinate_publication "$publisher"; then
        # An older archive can roll back a newer installation. Its own
        # installer is required when there is no installed coordinator.
        publisher=$PREFIX/current/bin/cagents
        owned_prefix && [ -x "$publisher" ] && can_coordinate_publication "$publisher" \
            || fail 'use the release installer published with this archive'
    fi
    set -- --prefix "$PREFIX" --version "$VERSION" --publish-tree "$NEW" --publisher "$publisher"
    "$publisher" --no-start coordinate-install --prefix "$PREFIX" \
        --installer "$0" --publish-only -- "$@"

    # Start the selected version through normal daemon admission after the
    # coordinator and publisher have released both fences. The plist is for
    # the next login.
    DAEMON_STARTED=0
    if [ "$START" -eq 1 ]; then
        if selected_bin=$(CDPATH= cd -P "$PREFIX/current/bin" && pwd -P) \
            && "$selected_bin/cagents" daemon start >/dev/null; then
            DAEMON_STARTED=1
        else
            note 'could not start the daemon; it will start when you open cagents'
        fi
    fi

    if [ "$DAEMON_STARTED" -eq 1 ]; then
        printf 'Installed cagents %s; the daemon is running and will start at login.\n' "$VERSION"
    else
        printf 'Installed cagents %s; the daemon will start at login or when you open cagents.\n' "$VERSION"
    fi
    printf 'Open cagents from Applications, or run: cagents ui\n'
    case :$PATH: in
        *:"$HOME/.local/bin":*) ;;
        *) printf 'Add %s to PATH to use the cagents command.\n' "$HOME/.local/bin" ;;
    esac
    exit 0
fi

# Only the guarded publication shell reaches the installation.
case $VERSION in
    ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "invalid version: $VERSION" ;;
esac
case $PUBLISH_TREE in
    /*) ;;
    *) fail 'publication needs an absolute release tree path' ;;
esac
case $PUBLISHER in
    "$PUBLISH_TREE/bin/cagents"|"$PREFIX/current/bin/cagents") ;;
    *) fail 'publication needs a staged or installed coordinator' ;;
esac
require_publication_fence "$PUBLISHER"

if [ -e "$PREFIX" ] || [ -L "$PREFIX" ]; then
    [ -d "$PREFIX" ] && [ ! -L "$PREFIX" ] || fail "$PREFIX is not a directory"
    if ! owned_prefix; then
        entries=$(ls -A "$PREFIX") || fail "cannot inspect $PREFIX"
        case $entries in
            '') ;;
            "${PENDING_MARKER##*/}")
                # An interrupted first install can leave this one file.
                [ -f "$PENDING_MARKER" ] && [ ! -L "$PENDING_MARKER" ] \
                    && [ "$(stat -f %u "$PENDING_MARKER")" = "$(id -u)" ] \
                    || fail "$PENDING_MARKER is not an installation marker"
                ;;
            *) fail "$PREFIX holds files this installer did not make; choose another --prefix" ;;
        esac
    fi
fi
mkdir -p "$PREFIX"
[ "$(stat -f %u "$PREFIX")" = "$(id -u)" ] || fail "$PREFIX is not owned by you"

STAGE=
# The login item is staged beside the one it replaces (see AGENT_STAGE).
AGENT_STAGE=
# Until the install starts changing what is published, there is nothing to
# roll back; see roll_back below.
PUBLISHING=0
STAGE_KEPT=0
roll_back() { :; }
cleanup() {
    # Nothing may stop a rollback halfway: not a second interrupt, nor a
    # message to a terminal or pipe that has gone.
    set +e
    trap '' HUP INT TERM PIPE
    roll_back
    rm -f "$PREFIX/.current.$$"
    [ -z "$STAGE" ] || [ "$STAGE_KEPT" -eq 1 ] || rm -rf "$STAGE"
    if [ -n "$AGENT_STAGE" ]; then
        rm -f "$AGENT_STAGE/new"
        [ "$STAGE_KEPT" -eq 1 ] && exists "$AGENT_STAGE/prior" || rm -rf "$AGENT_STAGE"
    fi
}
exists() { [ -e "$1" ] || [ -L "$1" ]; }
trap cleanup EXIT
trap 'trap "" HUP INT TERM; exit 1' HUP INT TERM

# Check the selected installation and daemon while new launches are fenced.
CURRENT_VERSION=
if [ -L "$PREFIX/current" ]; then
    CURRENT_VERSION=$(readlink "$PREFIX/current")
    CURRENT_VERSION=${CURRENT_VERSION#versions/}
fi
daemon=$(installed_daemon "$PUBLISH_TREE/bin/cagents") \
    || fail 'cannot tell whether the cagents daemon is running; the installation was not changed'
if [ -n "$daemon" ]; then
    printf '%s: the cagents daemon (%s) is running; the installation was not changed.\n' "$PROGRAM" "$daemon" >&2
    stop_instructions >&2
    exit 1
fi

# Commit ownership before staging under a fresh prefix, so an interrupted copy
# cannot leave a directory that the next installer refuses to use.
if ! owned_prefix; then
    rm -f "$PENDING_MARKER"
    (set -C; printf '%s\n' "$MARKER_TEXT" > "$PENDING_MARKER")
    mv -f "$PENDING_MARKER" "$MARKER"
fi
STAGE=$(mktemp -d "$PREFIX/.stage.XXXXXX")
NEW=$STAGE/candidate
# Rollback and publication use a child-owned stage on the prefix's volume.
# The original download can disappear once this copy is complete.
ditto "$PUBLISH_TREE" "$NEW" || fail 'could not stage the verified release on the installation volume'

mkdir -p "$PREFIX/versions"
chmod 0755 "$PREFIX"
TARGET_DIR=$PREFIX/versions/$VERSION
mkdir -p "$HOME/Applications" "$HOME/.local/bin" "$HOME/Library/LaunchAgents" \
    || fail 'cannot create the folders the installation links from; the installation was not changed'
# The new login item and a copy of the one it replaces are staged in a
# hidden folder beside it, so that publishing and restoring it are renames
# within one volume, whatever the prefix. launchd does not look inside it.
AGENT_STAGE=$(mktemp -d "$HOME/Library/LaunchAgents/.cagents-install.XXXXXX") \
    || fail 'cannot stage the login item; the installation was not changed'
AGENT_NEW=$AGENT_STAGE/new
AGENT_PRIOR=$AGENT_STAGE/prior

# Everything the installation publishes is prepared in the stage first: the
# login item, and the stable bundle as it will be. Nothing outside the stage
# changes until the release is in place and answers from there.
#
# The login item runs `cagents daemon start` once at login, as the XDG
# autostart entry does on Linux. Resolve the selected bin directory before
# exec, so the CLI finds companions beside its own executable even if
# another update switches current after launch.
cat > "$AGENT_NEW" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>-c</string>
		<string>$(printf '%s' "$LOGIN_COMMAND" | sed 's/&/\&amp;/g')</string>
		<string>$LOGIN_NAME</string>
		<string>$(printf '%s' "$PREFIX" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g')</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>AbandonProcessGroup</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>/dev/null</string>
	<key>StandardErrorPath</key>
	<string>/dev/null</string>
</dict>
</plist>
EOF
plutil -lint -s "$AGENT_NEW" && chmod 0644 "$AGENT_NEW" \
    || fail 'could not write the login item; the installation was not changed'
# Cagents.app stays at one path, which the Dock and Login Items remember;
# it is replaced only when the release changed it.
APP_CHANGED=0
if [ ! -d "$APP" ] || [ -L "$APP" ] || ! release_executable "$APP/$APP_LAUNCHER" \
    || ! diff -rq "$NEW/Cagents.app" "$APP" >/dev/null 2>&1; then
    APP_CHANGED=1
    ditto "$NEW/Cagents.app" "$STAGE/Cagents.app" \
        && codesign --verify --strict "$STAGE/Cagents.app" 2>/dev/null \
        || fail 'could not prepare Cagents.app; the installation was not changed'
fi

# The version kept for rollback: the one this install replaces, or after a
# reinstall the one recorded before. Publishing it commits the install.
PREVIOUS_VERSION=$(cat "$PREFIX/.previous" 2>/dev/null || :)
if [ -n "$CURRENT_VERSION" ] && [ "$CURRENT_VERSION" != "$VERSION" ]; then
    PREVIOUS_VERSION=$CURRENT_VERSION
fi
[ "$PREVIOUS_VERSION" != "$VERSION" ] || PREVIOUS_VERSION=
printf '%s\n' "$PREVIOUS_VERSION" > "$STAGE/previous" \
    || fail 'cannot record the version kept for rollback; the installation was not changed'

# A failure before the install commits puts each change back in reverse:
# the login item, the links, the bundle, `current` and the version tree.
# Each step is a rename or a link, and the rollback reads what each one left
# rather than a record of it, so an interrupt between a step and anything
# after it cannot mislead it.
PRIOR_CURRENT=
[ ! -L "$PREFIX/current" ] || PRIOR_CURRENT=$(readlink "$PREFIX/current")
PRIOR_BIN_LINK=
[ ! -L "$BIN_LINK" ] || PRIOR_BIN_LINK=$(readlink "$BIN_LINK")
PRIOR_APP_LINK=
[ ! -L "$APP_LINK" ] || PRIOR_APP_LINK=$(readlink "$APP_LINK")
# Each is published by renaming a staged file over it, and a rename onto a
# folder or link would move the file inside or past it instead.
for published in "$AGENT_PLIST" "$PREFIX/.previous"; do
    if exists "$published" && { [ -L "$published" ] || [ ! -f "$published" ]; }; then
        fail "$published is not a file this installer made; remove it, then run this installer again"
    fi
done
PRIOR_AGENT=0
if [ -f "$AGENT_PLIST" ]; then
    cp "$AGENT_PLIST" "$AGENT_PRIOR" \
        || fail 'cannot keep a copy of the login item; the installation was not changed'
    PRIOR_AGENT=1
fi
TREE=kept
put_back_link() { # LINK PRIOR-TARGET
    if [ -n "$2" ]; then
        [ -L "$1" ] && [ "$(readlink "$1")" = "$2" ] && return 0
        rm -f "$1" && ln -s "$2" "$1"
    elif [ -L "$1" ]; then
        rm -f "$1"
    fi
}
roll_back() {
    # Committed once the staged record of the kept version was published.
    [ "$PUBLISHING" -eq 1 ] && exists "$STAGE/previous" || return 0
    ok=1
    # Only a published login item consumed the staged one.
    if ! exists "$AGENT_NEW"; then
        if [ "$PRIOR_AGENT" -eq 1 ]; then
            mv -f "$AGENT_PRIOR" "$AGENT_PLIST" || ok=0
        else
            rm -f "$AGENT_PLIST" || ok=0
        fi
    fi
    put_back_link "$APP_LINK" "$PRIOR_APP_LINK" || ok=0
    put_back_link "$BIN_LINK" "$PRIOR_BIN_LINK" || ok=0
    if [ "$APP_CHANGED" -eq 1 ]; then
        # The staged bundle is gone once it was moved into place; the one it
        # replaced is in the stage once it was moved aside.
        exists "$STAGE/Cagents.app" || rm -rf "$APP" || ok=0
        if exists "$STAGE/Cagents.app.prior"; then
            if exists "$APP"; then ok=0; else mv "$STAGE/Cagents.app.prior" "$APP" || ok=0; fi
        fi
    fi
    if [ -n "$PRIOR_CURRENT" ]; then
        if [ "$(readlink "$PREFIX/current" 2>/dev/null || :)" != "$PRIOR_CURRENT" ]; then
            { rm -f "$PREFIX/.current.$$" \
                && ln -s "$PRIOR_CURRENT" "$PREFIX/.current.$$" \
                && mv -fh "$PREFIX/.current.$$" "$PREFIX/current"; } || ok=0
        fi
    elif [ -L "$PREFIX/current" ]; then
        rm -f "$PREFIX/current" || ok=0
    fi
    # The unpacked release is gone from the stage once it was moved into
    # place; the tree it replaced is in the stage once it was set aside.
    if [ "$TREE" != kept ] && ! exists "$NEW"; then
        rm -rf "$TARGET_DIR" || ok=0
    fi
    if [ "$TREE" = replacing ] && exists "$STAGE/replaced"; then
        if exists "$TARGET_DIR"; then ok=0; else mv "$STAGE/replaced" "$TARGET_DIR" || ok=0; fi
    fi
    if [ "$ok" -eq 1 ]; then
        note 'rolled back; the previous installation is unchanged'
    else
        STAGE_KEPT=1
        note "could not fully roll back; the files this install replaced are in $STAGE"
        ! exists "$AGENT_PRIOR" || note "and the login item it replaced is $AGENT_PRIOR"
    fi
}
PUBLISHING=1

if [ -e "$TARGET_DIR" ] || [ -L "$TARGET_DIR" ]; then
    # This release again, as a repair or a return to the version kept for
    # rollback: keep the tree when it is the release's, or replace it whole.
    # A process running from the old tree keeps the files it has open.
    if ! same_release_tree "$NEW" "$TARGET_DIR"; then
        TREE=replacing
        mv "$TARGET_DIR" "$STAGE/replaced" || fail 'cannot set aside the version tree it replaces'
        mv "$NEW" "$TARGET_DIR" || fail 'cannot move the release into place'
    fi
else
    TREE=added
    mv "$NEW" "$TARGET_DIR" || fail 'cannot move the release into place'
fi
# Ready means every program answers from where it will run.
verify_program_versions "$TARGET_DIR"

{ rm -f "$PREFIX/.current.$$" && ln -s "versions/$VERSION" "$PREFIX/.current.$$" \
    && mv -fh "$PREFIX/.current.$$" "$PREFIX/current"; } || fail 'cannot select the new version'
if [ "$APP_CHANGED" -eq 1 ]; then
    if [ -e "$APP" ] || [ -L "$APP" ]; then
        mv "$APP" "$STAGE/Cagents.app.prior" || fail 'cannot replace Cagents.app'
    fi
    mv "$STAGE/Cagents.app" "$APP" || fail 'cannot replace Cagents.app'
fi
link() {
    if [ -L "$2" ]; then
        rm -f "$2" || return 1
    elif [ -e "$2" ]; then
        note "kept the existing $2; it is not a link this installer made"
        return 0
    fi
    ln -s "$1" "$2"
}
link "$APP" "$APP_LINK" || fail "cannot link $APP_LINK"
link "$PREFIX/current/bin/cagents" "$BIN_LINK" || fail "cannot link $BIN_LINK"
mv -f "$AGENT_NEW" "$AGENT_PLIST" || fail 'cannot install the login item'
# The commit.
mv -f "$STAGE/previous" "$PREFIX/.previous" || fail 'cannot record the version kept for rollback'
[ "$APP_CHANGED" -eq 0 ] || register_app -f "$APP"

# Keep the served version and the one it replaced, for rollback; remove
# older trees no running process uses. Without a reliable answer from lsof,
# remove none.
in_use=unknown
if command -v lsof >/dev/null 2>&1; then
    status=0
    lsof -a -u "$(id -u)" -d txt -Fn > "$STAGE/lsof" 2>/dev/null || status=$?
    # lsof exits 1 when a selection matches nothing, which here (this shell
    # itself runs from an executable) means it could not see the processes.
    if [ "$status" -eq 0 ] && [ -s "$STAGE/lsof" ]; then
        in_use=$(awk -v root="$PREFIX/versions/" '
            substr($0, 1, 1) == "n" && index(substr($0, 2), root) == 1 {
                rest = substr($0, 2 + length(root)); sub(/\/.*/, "", rest); print rest
            }' "$STAGE/lsof" | sort -u)
    fi
fi
if [ "$in_use" != unknown ]; then
    for tree in "$PREFIX/versions"/*; do
        name=${tree##*/}
        [ "$name" != "$VERSION" ] && [ "$name" != "$PREVIOUS_VERSION" ] || continue
        [ -d "$tree" ] && [ ! -L "$tree" ] || continue
        printf '%s\n' "$in_use" | grep -qxF "$name" && continue
        rm -rf "$tree"
    done
fi
# Retire this installation's previous login job during publication, so a
# concurrent removal cannot be followed by loading a stale job.
unload_agent
CAGENTS_INSTALLER

/bin/sh "$installer_dir/install.sh" "$@"
