#!/bin/sh
set -eu

PROGRAM=cagents-install
DEFAULT_REPOSITORY='<OWNER>/<REPO>'

usage() {
    cat <<'EOF'
Usage: install.sh [OPTIONS]

Install the cagents Linux x86_64 release for the current user.

Options:
  --version VERSION  Install a release version (default: latest v* release)
  --prefix PATH      Installation root (default: $XDG_DATA_HOME/cagents-install)
  --restart-daemon   Checkpoint supported sessions and replace the running daemon
  --relaunch-agents  With --restart-daemon, accept fresh launches for non-resumable agents
  --uninstall        Remove program files while preserving user data
  --purge            Uninstall and also remove cagents user data
  --help             Show this help

Environment:
  CAGENTS_REPOSITORY  GitHub OWNER/REPO used for downloads
  CAGENTS_ARCHIVE     Local release archive (requires or infers --version)
  CAGENTS_CHECKSUMS   Local SHA256SUMS manifest
EOF
}

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

VERSION=
PREFIX=
UNINSTALL=0
PURGE=0
RESTART_DAEMON=0
RELAUNCH_AGENTS=0

while [ "$#" -gt 0 ]; do
    case $1 in
        --version)
            [ "$#" -ge 2 ] || fail '--version requires a value'
            VERSION=$2
            shift 2
            ;;
        --prefix)
            [ "$#" -ge 2 ] || fail '--prefix requires a path'
            PREFIX=$2
            shift 2
            ;;
        --restart-daemon)
            RESTART_DAEMON=1
            shift
            ;;
        --relaunch-agents)
            RELAUNCH_AGENTS=1
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --purge)
            UNINSTALL=1
            PURGE=1
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[ "$RELAUNCH_AGENTS" -eq 0 ] || [ "$RESTART_DAEMON" -eq 1 ] \
    || fail '--relaunch-agents requires --restart-daemon'

[ "$RESTART_DAEMON" -eq 0 ] || [ "$UNINSTALL" -eq 0 ] \
    || fail '--restart-daemon cannot be combined with --uninstall or --purge'

if [ "$(id -u)" -eq 0 ]; then
    fail 'refusing to run as root; install cagents from your user account'
fi

[ "$(uname -s)" = Linux ] || fail 'only Linux is supported'
case $(uname -m) in
    x86_64) ;;
    *) fail 'only x86_64 Linux is supported' ;;
esac
case $(getconf GNU_LIBC_VERSION 2>/dev/null || :) in
    glibc\ *) ;;
    *) fail 'the x86_64 glibc release requires a glibc system' ;;
esac

: "${HOME:?HOME is not set}"
case $HOME in
    /*) ;;
    *) fail 'HOME must be an absolute path' ;;
esac

DATA_HOME=${XDG_DATA_HOME:-"$HOME/.local/share"}
case $DATA_HOME in
    /*) ;;
    *) fail 'XDG_DATA_HOME must be an absolute path' ;;
esac
CONFIG_HOME=${XDG_CONFIG_HOME:-"$HOME/.config"}
case $CONFIG_HOME in
    /*) ;;
    *) fail 'XDG_CONFIG_HOME must be an absolute path' ;;
esac

if [ -z "$PREFIX" ]; then
    PREFIX=$DATA_HOME/cagents-install
fi
case $PREFIX in
    /*) ;;
    *) fail '--prefix must be an absolute path' ;;
esac
while [ "$PREFIX" != / ] && [ "${PREFIX%/}" != "$PREFIX" ]; do
    PREFIX=${PREFIX%/}
done
case $PREFIX in
    *'
'*) fail '--prefix must not contain a newline' ;;
esac
[ "$PREFIX" != / ] || fail 'refusing to use / as the installation root'

remove_owned_link() {
    link=$1
    if [ -L "$link" ]; then
        target=$(readlink "$link") || fail "cannot inspect $link"
        case $target in
            "$PREFIX/current"/*) rm -f "$link" ;;
        esac
    fi
}

read_daemon_status() {
    daemon_cli=$1
    if daemon_output=$("$daemon_cli" --json daemon status 2>/dev/null); then
        case $daemon_output in
            *'
'*) DAEMON_STATUS=unknown ;;
            '{"ok":true,"command":"daemon.status","data":{'*'"running":true'*|\
            '{"command":"daemon.status","data":{'*'"running":true,'*'},"ok":true}')
                DAEMON_STATUS=running
                ;;
            '{"ok":true,"command":"daemon.status","data":{'*'"running":false'*|\
            '{"command":"daemon.status","data":{'*'"running":false,'*'},"ok":true}')
                DAEMON_STATUS=stopped
                ;;
            *) DAEMON_STATUS=unknown ;;
        esac
    else
        DAEMON_STATUS=unknown
    fi
}

stop_daemon_and_wait() {
    daemon_cli=$1
    read_daemon_status "$daemon_cli"
    case $DAEMON_STATUS in
        stopped) return 0 ;;
        running) ;;
        *) fail 'cannot verify daemon status; installation was not changed' ;;
    esac

    "$daemon_cli" daemon stop --require-empty >/dev/null \
        || fail 'daemon was not stopped: finish terminals or explicitly confirm their termination with cagents daemon stop --confirm, then retry; installation was not changed'

    wait_for_daemon_shutdown "$daemon_cli"
}

wait_for_daemon_shutdown() {
    daemon_cli=$1
    daemon_wait=30
    while [ "$daemon_wait" -gt 0 ]; do
        read_daemon_status "$daemon_cli"
        [ "$DAEMON_STATUS" != stopped ] || return 0
        daemon_wait=$((daemon_wait - 1))
        [ "$daemon_wait" -eq 0 ] || sleep 1
    done
    fail 'could not verify that the daemon stopped; installation was not changed'
}

prepare_daemon_upgrade() {
    daemon_cli=$1
    # Discover support without issuing an unknown mutation to a legacy CLI.
    if ! daemon_help=$("$daemon_cli" daemon --help 2>/dev/null) \
        || ! printf '%s\n' "$daemon_help" | awk '
            $1 == "prepare-upgrade" { supported = 1 }
            END { exit !supported }
        '; then
        fail 'the installed CLI cannot checkpoint upgrades; for the one-time legacy bootstrap, finish sessions or explicitly terminate disposable sessions with the installed CLI, then retry after its daemon exits; installation was not changed'
    fi
    set -- --yes
    if [ "$RELAUNCH_AGENTS" -eq 1 ]; then
        relaunch_help=$("$daemon_cli" daemon prepare-upgrade --help 2>/dev/null) \
            || fail 'cannot inspect installed upgrade support; installation was not changed'
        printf '%s\n' "$relaunch_help" | awk '$1 == "--relaunch-agents" { supported = 1 } END { exit !supported }' \
            || fail 'the installed CLI cannot record agent relaunch acceptance; finish blocking sessions before upgrading; installation was not changed'
        set -- "$@" --relaunch-agents
    fi
    "$daemon_cli" --json daemon prepare-upgrade "$@" >/dev/null \
        || fail 'daemon checkpoint preparation failed; installation was not changed'
    wait_for_daemon_shutdown "$daemon_cli"
    DAEMON_STOPPED_FOR_SWITCH=1
    DAEMON_CHECKPOINTED=1
}

uninstall_cagents() {
    if [ -e "$PREFIX/.cagents-install-root" ]; then
        [ -f "$PREFIX/.cagents-install-root" ] && [ ! -L "$PREFIX/.cagents-install-root" ] \
            || fail "unsafe installation marker at $PREFIX"
        [ "$(cat "$PREFIX/.cagents-install-root")" = 'cagents install root' ] \
            || fail "unrecognized installation marker at $PREFIX"

        if [ -L "$PREFIX/current" ]; then
            stop_daemon_and_wait "$PREFIX/current/bin/cagents"
        elif [ -e "$PREFIX/current" ]; then
            fail "refusing to remove non-symlink $PREFIX/current"
        fi

        remove_owned_link "$HOME/.local/bin/cagents"
        remove_owned_link "$DATA_HOME/applications/dev.cagents.Cagents.desktop"
        remove_owned_link "$DATA_HOME/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"
        remove_owned_link "$DATA_HOME/icons/hicolor/128x128/apps/dev.cagents.Cagents.png"
        remove_owned_link "$DATA_HOME/metainfo/dev.cagents.Cagents.metainfo.xml"
        remove_owned_link "$CONFIG_HOME/autostart/dev.cagents.Cagents.Daemon.desktop"

        rm -f "$PREFIX/current"
        rm -rf "$PREFIX/versions"
        rm -f "$PREFIX/.cagents-install-root"
        rmdir "$PREFIX" 2>/dev/null || :
    else
        printf '%s: no install.sh installation found at %s\n' "$PROGRAM" "$PREFIX" >&2
    fi

    if [ "$PURGE" -eq 1 ]; then
        CACHE_HOME=${XDG_CACHE_HOME:-"$HOME/.cache"}
        case $CACHE_HOME in
            /*) ;;
            *) fail 'XDG_CACHE_HOME must be an absolute path' ;;
        esac
        rm -rf "$CONFIG_HOME/cagents" "$DATA_HOME/cagents" "$CACHE_HOME/cagents"
        printf 'cagents program files and user data have been removed.\n'
    else
        printf 'cagents program files have been removed; user data was preserved.\n'
    fi
}

if [ "$UNINSTALL" -eq 1 ]; then
    uninstall_cagents
    exit 0
fi

case $VERSION in
    v*) VERSION=${VERSION#v} ;;
esac

LOCAL_ARCHIVE=${CAGENTS_ARCHIVE:-}
LOCAL_CHECKSUMS=${CAGENTS_CHECKSUMS:-}
if [ -n "$LOCAL_ARCHIVE" ]; then
    [ -f "$LOCAL_ARCHIVE" ] && [ -r "$LOCAL_ARCHIVE" ] || fail 'CAGENTS_ARCHIVE is not a readable regular file'
    if [ -z "$VERSION" ]; then
        archive_base=${LOCAL_ARCHIVE##*/}
        case $archive_base in
            cagents-*-x86_64-unknown-linux-gnu.tar.gz)
                VERSION=${archive_base#cagents-}
                VERSION=${VERSION%-x86_64-unknown-linux-gnu.tar.gz}
                ;;
            *) fail '--version is required when CAGENTS_ARCHIVE does not use the release filename' ;;
        esac
    fi
fi
if [ -n "$LOCAL_CHECKSUMS" ]; then
    [ -f "$LOCAL_CHECKSUMS" ] && [ -r "$LOCAL_CHECKSUMS" ] || fail 'CAGENTS_CHECKSUMS is not a readable regular file'
fi

REPOSITORY=${CAGENTS_REPOSITORY:-$DEFAULT_REPOSITORY}
validate_repository() {
    case $REPOSITORY in
        '<OWNER>/<REPO>'|'') fail 'set CAGENTS_REPOSITORY to the GitHub OWNER/REPO for this project' ;;
        *[!0-9A-Za-z_./-]*) fail 'CAGENTS_REPOSITORY contains unsafe characters' ;;
    esac
    owner=${REPOSITORY%%/*}
    repository_name=${REPOSITORY#*/}
    [ -n "$owner" ] && [ -n "$repository_name" ] && [ "$repository_name" = "${repository_name#*/}" ] \
        || fail 'CAGENTS_REPOSITORY must have the form OWNER/REPO'
}

need_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

need_command awk
need_command mktemp
need_command sed
need_command tar
need_command readlink

CURL_OPTIONS="--proto =https --tlsv1.2 --fail --location --silent --show-error"
AUTHENTICATED_GH=0
if [ -z "$VERSION" ] || [ -z "$LOCAL_ARCHIVE" ] || [ -z "$LOCAL_CHECKSUMS" ]; then
    validate_repository
    if command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1; then
        AUTHENTICATED_GH=1
    elif [ "$REPOSITORY" = CarsonBurke/cagents ]; then
        fail 'private release access requires an authenticated GitHub CLI account; run gh auth login --hostname github.com with access to CarsonBurke/cagents; installation was not changed'
    fi
fi
if [ -z "$VERSION" ]; then
    if [ "$AUTHENTICATED_GH" -eq 1 ]; then
        latest_tag=$(gh release view --repo "github.com/$REPOSITORY" --json tagName --jq .tagName) \
            || fail 'could not resolve the latest release with the authenticated GitHub account'
        case $latest_tag in
            v*) VERSION=${latest_tag#v} ;;
            *) fail 'latest release must have a v* tag' ;;
        esac
    else
        need_command curl
        latest_url=$(curl $CURL_OPTIONS -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest") \
            || fail 'could not resolve the public release; for private repositories, authenticate with gh auth login --hostname github.com'
        case $latest_url in
            https://github.com/"$REPOSITORY"/releases/tag/v*) VERSION=${latest_url##*/v} ;;
            *) fail 'latest release did not redirect to a v* tag' ;;
        esac
    fi
fi
case $VERSION in
    ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "unsafe release version: $VERSION" ;;
esac

ARCHIVE_NAME=cagents-$VERSION-x86_64-unknown-linux-gnu.tar.gz
TOP_LEVEL=cagents-$VERSION
TEMP_ROOT=${TMPDIR:-/tmp}
case $TEMP_ROOT in
    /*) ;;
    *) fail 'TMPDIR must be an absolute path' ;;
esac
DOWNLOAD_DIR=$(mktemp -d "$TEMP_ROOT/cagents-download.XXXXXX")
INSTALL_STAGE=
NEW_VERSION_DIR=
CURRENT_TEMP=
CURRENT_SWITCHED=0
TRANSACTION_COMMITTED=0
HAD_CURRENT=0
PREVIOUS_CURRENT_TARGET=
ROLLBACK_OK=1
DAEMON_STOPPED_FOR_SWITCH=0
DAEMON_CHECKPOINTED=0
OLD_DAEMON_WAS_RUNNING=0
DAEMON_STARTED=0
DAEMON_START_ATTEMPTED=0
BIN_LINK=$HOME/.local/bin/cagents
DESKTOP_LINK=$DATA_HOME/applications/dev.cagents.Cagents.desktop
ICON_LINK=$DATA_HOME/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg
RASTER_ICON_LINK=$DATA_HOME/icons/hicolor/128x128/apps/dev.cagents.Cagents.png
METAINFO_LINK=$DATA_HOME/metainfo/dev.cagents.Cagents.metainfo.xml
AUTOSTART_LINK=$CONFIG_HOME/autostart/dev.cagents.Cagents.Daemon.desktop
BIN_TARGET=$PREFIX/current/bin/cagents
DESKTOP_TARGET=$PREFIX/current/share/applications/dev.cagents.Cagents.desktop
ICON_TARGET=$PREFIX/current/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg
RASTER_ICON_TARGET=$PREFIX/current/share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png
METAINFO_TARGET=$PREFIX/current/share/metainfo/dev.cagents.Cagents.metainfo.xml
AUTOSTART_TARGET=$PREFIX/current/share/autostart/dev.cagents.Cagents.Daemon.desktop
BIN_LINK_CREATED=0
DESKTOP_LINK_CREATED=0
ICON_LINK_CREATED=0
RASTER_ICON_LINK_CREATED=0
METAINFO_LINK_CREATED=0
AUTOSTART_LINK_CREATED=0
cleanup() {
    cleanup_status=$?
    rm -rf "$DOWNLOAD_DIR" || :
    if [ -n "$INSTALL_STAGE" ]; then
        rm -rf "$INSTALL_STAGE" || :
    fi
    if [ -n "$CURRENT_TEMP" ]; then
        rm -f "$CURRENT_TEMP" || :
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$DAEMON_CHECKPOINTED" -eq 1 ] \
        && [ "$DAEMON_START_ATTEMPTED" -eq 1 ]; then
        read_daemon_status "$NEW_VERSION_DIR/bin/cagents"
        if [ "$DAEMON_STATUS" != stopped ]; then
            # This also fences interruption while daemon start is in flight.
            # A possible replacement writer must retain its own executables.
            TRANSACTION_COMMITTED=1
            printf '%s: replacement daemon shutdown is not proven; retained the new installation and recovery state; inspect cagents daemon status before retrying\n' \
                "$PROGRAM" >&2
        fi
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] && [ "$CURRENT_SWITCHED" -eq 1 ]; then
        if [ "$HAD_CURRENT" -eq 1 ]; then
            rollback_current=$PREFIX/.rollback.$$
            rm -f "$rollback_current" || ROLLBACK_OK=0
            if [ "$ROLLBACK_OK" -eq 1 ] \
                && ln -s "$PREVIOUS_CURRENT_TARGET" "$rollback_current" \
                && mv -Tf "$rollback_current" "$PREFIX/current"; then
                :
            else
                ROLLBACK_OK=0
                rm -f "$rollback_current" || :
                printf '%s: failed to restore the previous current version at %s\n' \
                    "$PROGRAM" "$PREFIX" >&2
            fi
        else
            rm -f "$PREFIX/current" || ROLLBACK_OK=0
        fi
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$DAEMON_STOPPED_FOR_SWITCH" -eq 1 ] \
        && [ "$OLD_DAEMON_WAS_RUNNING" -eq 1 ] \
        && [ "$ROLLBACK_OK" -eq 1 ]; then
        "$PREFIX/current/bin/cagents" daemon start >/dev/null 2>&1 \
            || printf '%s: restored the previous version but could not restart its daemon\n' \
                "$PROGRAM" >&2
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$BIN_LINK_CREATED" -eq 1 ] && [ -L "$BIN_LINK" ] \
        && [ "$(readlink "$BIN_LINK" 2>/dev/null || :)" = "$BIN_TARGET" ]; then
        rm -f "$BIN_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$DESKTOP_LINK_CREATED" -eq 1 ] && [ -L "$DESKTOP_LINK" ] \
        && [ "$(readlink "$DESKTOP_LINK" 2>/dev/null || :)" = "$DESKTOP_TARGET" ]; then
        rm -f "$DESKTOP_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ICON_LINK_CREATED" -eq 1 ] && [ -L "$ICON_LINK" ] \
        && [ "$(readlink "$ICON_LINK" 2>/dev/null || :)" = "$ICON_TARGET" ]; then
        rm -f "$ICON_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$RASTER_ICON_LINK_CREATED" -eq 1 ] && [ -L "$RASTER_ICON_LINK" ] \
        && [ "$(readlink "$RASTER_ICON_LINK" 2>/dev/null || :)" = "$RASTER_ICON_TARGET" ]; then
        rm -f "$RASTER_ICON_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$METAINFO_LINK_CREATED" -eq 1 ] && [ -L "$METAINFO_LINK" ] \
        && [ "$(readlink "$METAINFO_LINK" 2>/dev/null || :)" = "$METAINFO_TARGET" ]; then
        rm -f "$METAINFO_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$AUTOSTART_LINK_CREATED" -eq 1 ] && [ -L "$AUTOSTART_LINK" ] \
        && [ "$(readlink "$AUTOSTART_LINK" 2>/dev/null || :)" = "$AUTOSTART_TARGET" ]; then
        rm -f "$AUTOSTART_LINK" || :
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ -n "$NEW_VERSION_DIR" ] && [ "$ROLLBACK_OK" -eq 1 ]; then
        rm -rf "$NEW_VERSION_DIR" || :
    fi
    exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
umask 077
ARCHIVE_PATH=$DOWNLOAD_DIR/$ARCHIVE_NAME
CHECKSUMS_PATH=$DOWNLOAD_DIR/SHA256SUMS

download_release_asset() {
    asset_name=$1
    asset_path=$2
    if [ "$AUTHENTICATED_GH" -eq 1 ]; then
        gh release download "v$VERSION" --repo "github.com/$REPOSITORY" \
            --pattern "$asset_name" --output "$asset_path" \
            || fail "could not download $asset_name with the authenticated GitHub account"
    else
        need_command curl
        curl $CURL_OPTIONS -o "$asset_path" \
            "https://github.com/$REPOSITORY/releases/download/v$VERSION/$asset_name" \
            || fail "could not download public asset $asset_name; for private repositories, authenticate with gh auth login --hostname github.com"
    fi
}

if [ -n "$LOCAL_ARCHIVE" ]; then
    cp "$LOCAL_ARCHIVE" "$ARCHIVE_PATH"
else
    download_release_asset "$ARCHIVE_NAME" "$ARCHIVE_PATH"
fi
if [ -n "$LOCAL_CHECKSUMS" ]; then
    cp "$LOCAL_CHECKSUMS" "$CHECKSUMS_PATH"
else
    download_release_asset SHA256SUMS "$CHECKSUMS_PATH"
fi

if ! EXPECTED_HASH=$(awk -v target="$ARCHIVE_NAME" '
    NF == 0 { next }
    NF != 2 || length($1) != 64 || $1 !~ /^[0123456789abcdefABCDEF]+$/ { bad = 1; next }
    $2 == target { matches++; hash = tolower($1) }
    END {
        if (bad || matches != 1) exit 1
        print hash
    }
' "$CHECKSUMS_PATH"); then
    fail 'SHA256SUMS is malformed or does not contain exactly one archive checksum'
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_HASH=$(sha256sum "$ARCHIVE_PATH" | awk '{ print tolower($1) }')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_HASH=$(shasum -a 256 "$ARCHIVE_PATH" | awk '{ print tolower($1) }')
else
    fail 'sha256sum or shasum is required'
fi
[ "$ACTUAL_HASH" = "$EXPECTED_HASH" ] || fail 'archive SHA-256 verification failed'

ARCHIVE_LIST=$DOWNLOAD_DIR/archive.list
ARCHIVE_VERBOSE=$DOWNLOAD_DIR/archive.verbose
LC_ALL=C tar -tzf "$ARCHIVE_PATH" > "$ARCHIVE_LIST" || fail 'archive cannot be listed'
LC_ALL=C tar -tvzf "$ARCHIVE_PATH" > "$ARCHIVE_VERBOSE" || fail 'archive metadata cannot be listed'
awk '
    {
        type = substr($1, 1, 1)
        last = substr($NF, length($NF), 1)
        if (type != "d" && type != "-") exit 1
        if (type == "d" && last != "/") exit 1
        if (type == "-" && last == "/") exit 1
    }
' "$ARCHIVE_VERBOSE" || fail 'archive contains links, special files, or invalid entry types'
if ! awk -v top="$TOP_LEVEL" '
    function allowed(path) {
        return path == top "/" ||
            path == top "/bin/" ||
            path == top "/bin/cagents" ||
            path == top "/bin/cagents-ui" ||
            path == top "/bin/cagentsd" ||
            path == top "/bin/.cagents-ui-lease-v1" ||
            path == top "/bin/cagents-uninstall" ||
            path == top "/share/" ||
            path == top "/share/applications/" ||
            path == top "/share/applications/dev.cagents.Cagents.desktop" ||
            path == top "/share/autostart/" ||
            path == top "/share/autostart/dev.cagents.Cagents.Daemon.desktop" ||
            path == top "/share/icons/" ||
            path == top "/share/icons/hicolor/" ||
            path == top "/share/icons/hicolor/scalable/" ||
            path == top "/share/icons/hicolor/scalable/apps/" ||
            path == top "/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg" ||
            path == top "/share/icons/hicolor/128x128/" ||
            path == top "/share/icons/hicolor/128x128/apps/" ||
            path == top "/share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png" ||
            path == top "/share/metainfo/" ||
            path == top "/share/metainfo/dev.cagents.Cagents.metainfo.xml" ||
            path == top "/share/doc/" ||
            path == top "/share/doc/cagents/" ||
            path == top "/share/doc/cagents/LICENSE"
    }
    !allowed($0) || seen[$0]++ { exit 1 }
    $0 == top "/bin/cagents" { required[1] = 1 }
    $0 == top "/bin/cagents-ui" { required[2] = 1 }
    $0 == top "/bin/cagentsd" { required[3] = 1 }
    $0 == top "/bin/.cagents-ui-lease-v1" { required[10] = 1 }
    $0 == top "/bin/cagents-uninstall" { required[4] = 1 }
    $0 == top "/share/applications/dev.cagents.Cagents.desktop" { required[5] = 1 }
    $0 == top "/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg" { required[6] = 1 }
    $0 == top "/share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png" { required[9] = 1 }
    $0 == top "/share/metainfo/dev.cagents.Cagents.metainfo.xml" { required[7] = 1 }
    $0 == top "/share/autostart/dev.cagents.Cagents.Daemon.desktop" { required[10] = 1 }
    $0 == top "/share/doc/cagents/LICENSE" { required[8] = 1 }
    END {
        for (i = 1; i <= 10; i++) if (!required[i]) exit 1
    }
' "$ARCHIVE_LIST"; then
    fail 'archive has an unexpected or incomplete entry structure'
fi

if [ -L "$PREFIX" ]; then
    fail "refusing to install through symlink $PREFIX"
fi
if [ -e "$PREFIX" ] && [ ! -d "$PREFIX" ]; then
    fail "installation root is not a directory: $PREFIX"
fi
if [ -d "$PREFIX" ] && [ ! -e "$PREFIX/.cagents-install-root" ]; then
    if [ -n "$(find "$PREFIX" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
        fail "refusing to use non-empty unowned directory $PREFIX"
    fi
fi
mkdir -p "$PREFIX"
if [ -e "$PREFIX/.cagents-install-root" ]; then
    [ -f "$PREFIX/.cagents-install-root" ] && [ ! -L "$PREFIX/.cagents-install-root" ] \
        || fail "unsafe installation marker at $PREFIX"
    [ "$(cat "$PREFIX/.cagents-install-root")" = 'cagents install root' ] \
        || fail "unrecognized installation marker at $PREFIX"
else
    printf '%s\n' 'cagents install root' > "$PREFIX/.cagents-install-root"
fi
if [ -L "$PREFIX/versions" ] || { [ -e "$PREFIX/versions" ] && [ ! -d "$PREFIX/versions" ]; }; then
    fail "unsafe versions path at $PREFIX"
fi
mkdir -p "$PREFIX/versions"
[ ! -e "$PREFIX/versions/$VERSION" ] && [ ! -L "$PREFIX/versions/$VERSION" ] \
    || fail "version $VERSION is already installed at $PREFIX"
if [ -e "$PREFIX/current" ] && [ ! -L "$PREFIX/current" ]; then
    fail "refusing to replace non-symlink $PREFIX/current"
fi

if [ -L "$PREFIX/current" ]; then
    PREVIOUS_CURRENT_TARGET=$(readlink "$PREFIX/current") \
        || fail "cannot inspect $PREFIX/current"
    HAD_CURRENT=1
fi

check_link_destination() {
    link=$1
    expected_target=$2
    if [ -e "$link" ] || [ -L "$link" ]; then
        [ -L "$link" ] || fail "refusing to replace existing file $link"
        target=$(readlink "$link") || fail "cannot inspect $link"
        [ "$target" = "$expected_target" ] \
            || fail "refusing to replace unowned symlink $link"
    fi
}

prepare_link_parent() {
    link_directory=${1%/*}
    mkdir -p "$link_directory" || fail "cannot create link directory $link_directory"
    [ -d "$link_directory" ] || fail "link parent is not a directory: $link_directory"
}

desktop_exec_escape() {
    printf '%s' "$1" | sed \
        -e 's/\\/\\\\/g' \
        -e 's/"/\\"/g' \
        -e 's/`/\\`/g' \
        -e 's/\$/\\$/g' \
        -e 's/%/%%/g'
}

check_link_destination "$BIN_LINK" "$BIN_TARGET"
check_link_destination "$DESKTOP_LINK" "$DESKTOP_TARGET"
check_link_destination "$ICON_LINK" "$ICON_TARGET"
check_link_destination "$RASTER_ICON_LINK" "$RASTER_ICON_TARGET"
check_link_destination "$METAINFO_LINK" "$METAINFO_TARGET"
check_link_destination "$AUTOSTART_LINK" "$AUTOSTART_TARGET"
prepare_link_parent "$BIN_LINK"
prepare_link_parent "$DESKTOP_LINK"
prepare_link_parent "$ICON_LINK"
prepare_link_parent "$RASTER_ICON_LINK"
prepare_link_parent "$METAINFO_LINK"
prepare_link_parent "$AUTOSTART_LINK"

INSTALL_STAGE=$PREFIX/.staging.$$
(umask 077 && mkdir "$INSTALL_STAGE") || fail 'could not create installation staging directory'
tar --no-same-owner --no-same-permissions -xzf "$ARCHIVE_PATH" -C "$INSTALL_STAGE" \
    || fail 'archive extraction failed'
AUTOSTART_EXEC=$(desktop_exec_escape "$BIN_TARGET")
DESKTOP_EXEC=$(desktop_exec_escape "$BIN_TARGET")
DESKTOP_ICON=$PREFIX/current/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg
DESKTOP_FILE=$INSTALL_STAGE/$TOP_LEVEL/share/applications/dev.cagents.Cagents.desktop
{
    while IFS= read -r line || [ -n "$line" ]; do
        case $line in
            Exec=*) printf 'Exec="%s" ui\n' "$DESKTOP_EXEC" ;;
            TryExec=*) printf 'TryExec=%s\n' "$BIN_TARGET" ;;
            Icon=*) printf 'Icon=%s\n' "$DESKTOP_ICON" ;;
            *) printf '%s\n' "$line" ;;
        esac
    done
} < "$DESKTOP_FILE" > "$DESKTOP_FILE.rewritten" \
    || fail 'could not prepare the application desktop entry'
mv "$DESKTOP_FILE.rewritten" "$DESKTOP_FILE" \
    || fail 'could not replace the application desktop entry'
{
    printf '%s\n' \
        '[Desktop Entry]' \
        'Type=Application' \
        'Name=cagents daemon' \
        'Comment=Keep cagents sessions available in the system tray'
    printf 'Exec="%s" daemon start\n' "$AUTOSTART_EXEC"
    printf '%s\n' \
        'Terminal=false' \
        'NoDisplay=true' \
        'X-GNOME-Autostart-enabled=true'
} > "$INSTALL_STAGE/$TOP_LEVEL/share/autostart/dev.cagents.Cagents.Daemon.desktop" \
    || fail 'could not prepare the daemon autostart entry'
chmod 0755 "$INSTALL_STAGE/$TOP_LEVEL" "$INSTALL_STAGE/$TOP_LEVEL/bin"
chmod 0755 \
    "$INSTALL_STAGE/$TOP_LEVEL/bin/cagents" \
    "$INSTALL_STAGE/$TOP_LEVEL/bin/cagents-ui" \
    "$INSTALL_STAGE/$TOP_LEVEL/bin/cagentsd" \
    "$INSTALL_STAGE/$TOP_LEVEL/bin/cagents-uninstall"
find "$INSTALL_STAGE/$TOP_LEVEL/share" -type d -exec chmod 0755 {} +
find "$INSTALL_STAGE/$TOP_LEVEL/share" -type f -exec chmod 0644 {} +
NEW_VERSION_DIR=$PREFIX/versions/$VERSION
mv "$INSTALL_STAGE/$TOP_LEVEL" "$NEW_VERSION_DIR"
rmdir "$INSTALL_STAGE"
INSTALL_STAGE=

ensure_link() {
    link_target=$1
    link_path=$2
    link_kind=$3
    if [ -e "$link_path" ] || [ -L "$link_path" ]; then
        check_link_destination "$link_path" "$link_target"
        if [ "$link_kind" = desktop ]; then
            # Refresh directory watchers after switching the version target.
            ln -snf "$link_target" "$link_path" || fail "cannot refresh link $link_path"
        fi
        return
    fi

    case $link_kind in
        bin) BIN_LINK_CREATED=1 ;;
        desktop) DESKTOP_LINK_CREATED=1 ;;
        icon) ICON_LINK_CREATED=1 ;;
        raster_icon) RASTER_ICON_LINK_CREATED=1 ;;
        metainfo) METAINFO_LINK_CREATED=1 ;;
        autostart) AUTOSTART_LINK_CREATED=1 ;;
        *) fail 'internal error: unknown link kind' ;;
    esac
    ln -s "$link_target" "$link_path" || fail "cannot create link $link_path"
}


if [ "$HAD_CURRENT" -eq 1 ]; then
    if [ "$RESTART_DAEMON" -eq 1 ]; then
        read_daemon_status "$PREFIX/current/bin/cagents"
        case $DAEMON_STATUS in
            running)
                OLD_DAEMON_WAS_RUNNING=1
                prepare_daemon_upgrade "$PREFIX/current/bin/cagents"
                ;;
            stopped) ;;
            *) fail 'cannot verify the installed daemon status; installation was not changed' ;;
        esac
    else
        # The candidate handshake proves protocol compatibility without changing
        # the running process. Compatible updates never replace live PTYs.
        read_daemon_status "$NEW_VERSION_DIR/bin/cagents"
        case $DAEMON_STATUS in
            running)
                OLD_DAEMON_WAS_RUNNING=1
                ;;
            stopped) ;;
            *)
                read_daemon_status "$PREFIX/current/bin/cagents"
                [ "$DAEMON_STATUS" != running ] || OLD_DAEMON_WAS_RUNNING=1
                stop_daemon_and_wait "$PREFIX/current/bin/cagents"
                DAEMON_STOPPED_FOR_SWITCH=1
                ;;
        esac
    fi
fi
CURRENT_TEMP=$PREFIX/.current.$$
rm -f "$CURRENT_TEMP"
ln -s "versions/$VERSION" "$CURRENT_TEMP"
CURRENT_SWITCHED=1
mv -Tf "$CURRENT_TEMP" "$PREFIX/current"

# Publish launchers only after their executable and icon targets exist. Menus
# can cache a dangling desktop link as missing until their next full rescan.
ensure_link "$BIN_TARGET" "$BIN_LINK" bin
ensure_link "$DESKTOP_TARGET" "$DESKTOP_LINK" desktop
ensure_link "$ICON_TARGET" "$ICON_LINK" icon
ensure_link "$RASTER_ICON_TARGET" "$RASTER_ICON_LINK" raster_icon
ensure_link "$METAINFO_TARGET" "$METAINFO_LINK" metainfo
ensure_link "$AUTOSTART_TARGET" "$AUTOSTART_LINK" autostart
# A checkpointed update commits only after the replacement starts. Ordinary
# installs retain their existing login-autostart behavior on startup failure.
[ "$DAEMON_CHECKPOINTED" -eq 1 ] || TRANSACTION_COMMITTED=1
DAEMON_START_ATTEMPTED=1
if "$PREFIX/current/bin/cagents" daemon start >/dev/null; then
    DAEMON_STARTED=1
else
    if [ "$DAEMON_CHECKPOINTED" -eq 1 ]; then
        fail 'replacement daemon startup failed; checking rollback safety while preserving durable recovery state'
    fi
    printf '%s: installed cagents, but its production daemon could not start now; login autostart remains enabled\n' \
        "$PROGRAM" >&2
fi
TRANSACTION_COMMITTED=1
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force --ignore-theme-index "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || :
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || :
fi


if [ "$DAEMON_STARTED" -eq 1 ]; then
    printf 'Installed cagents %s; the production daemon is running and will start at login.\n' "$VERSION"
else
    printf 'Installed cagents %s; the production daemon will start at graphical login.\n' "$VERSION"
fi
printf 'Ensure %s is on PATH, then run: cagents ui\n' "$HOME/.local/bin"
