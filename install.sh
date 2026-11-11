#!/bin/sh
set -eu

# Recovery carries high descriptors that POSIX-only shells cannot close. Move
# that transport into Bash before running any utility; ordinary installs retain
# the portable sh path. The recovery worker must preflight this capability before
# asking its source to shut down.
if [ -n "${CAGENTS_UPDATE_RECOVERY_FD:-}" ]; then
    if [ "$CAGENTS_UPDATE_RECOVERY_FD" != 193 ]; then
        printf '%s\n' 'cagents-install: invalid update recovery descriptor' >&2
        exit 1
    fi
    if [ "${CAGENTS_DAEMON_LEASE_FD:-}" != 3 ] \
        || [ -z "${CAGENTS_UI_UPDATE_GUARD_FD:-}" ]; then
        printf '%s\n' 'cagents-install: update recovery requires the worker daemon lease and launch fence; keep the checkpoint and use the staged update worker' >&2
        exit 1
    fi
    # Compare the running interpreter itself; an exported BASH_VERSION can be
    # stale or supplied by a different shell.
    if ! [ /proc/$$/exe -ef /bin/bash ]; then
        if [ ! -x /bin/bash ]; then
            printf '%s\n' 'cagents-install: update recovery requires /bin/bash; keep the checkpoint and use the staged recovery build' >&2
            exit 1
        fi
        unset BASH_ENV ENV
        exec /bin/bash --noprofile --norc --posix "$0" "$@"
    fi
fi

installer_drop_recovery() {
    if [ "${CAGENTS_UPDATE_RECOVERY_FD:-}" = 193 ]; then
        # eval keeps this function parseable by ordinary POSIX sh. Recovery's
        # Bash bootstrap above owns these reserved slots, including the legacy
        # read fence; archive descriptors 190..192 are separate transport.
        eval 'exec 189<&- 193<&- 194<&- 195<&- 196<&-'
    fi
    unset CAGENTS_UPDATE_RECOVERY_FD
}

# Only explicit candidate lease validation, store maintenance and daemon start
# receive the ordinary daemon lease. Recovery authority reaches only the exact
# candidate daemon start, never probes, maintenance, or an older coordinator.
installer_lease_tool() (
    installer_drop_recovery
    exec "$@"
)
installer_tool() (
    if [ "${CAGENTS_DAEMON_LEASE_FD:-}" = 3 ]; then
        exec 3<&-
    fi
    unset CAGENTS_DAEMON_LEASE_FD
    installer_drop_recovery
    exec "$@"
)
installer_start_candidate() (
    if [ "${CAGENTS_UPDATE_RECOVERY_FD:-}" = 193 ]; then
        # current could have changed independently to an older CLI that cannot
        # seal this bundle. Only the extracted target may validate/forward it.
        exec "$NEW_VERSION_DIR/bin/cagents" daemon start
    fi
    exec "$PREFIX/current/bin/cagents" daemon start
)
if [ "${CAGENTS_DAEMON_LEASE_FD:-}" = 3 ] \
    || [ "${CAGENTS_UPDATE_RECOVERY_FD:-}" = 193 ]; then
    # Noninteractive Bash expands these aliases only in POSIX mode. Other
    # POSIX shells already expand them and may not offer the posix option.
    if (set -o posix) 2>/dev/null; then set -o posix; fi
    # Preserve command -v detection for optional and missing tools. Aliases
    # apply inside functions and command substitutions too.
    for installer_utility in awk cat chmod cp curl find getconf gh \
        gtk-update-icon-cache id ln mkdir mktemp mv readlink rm rmdir sed \
        sha256sum shasum sleep sync tar uname update-desktop-database; do
        if command -v "$installer_utility" >/dev/null 2>&1; then
            alias "$installer_utility=installer_tool $installer_utility"
        fi
    done
fi

PROGRAM=cagents-install
DEFAULT_REPOSITORY='<OWNER>/<REPO>'

usage() {
    cat <<'EOF'
Usage: install.sh [OPTIONS]

Install the cagents Linux x86_64 release for the current user.
When the running daemon is incompatible, install the files and defer its
replacement to the application unless --restart-daemon is explicit.

Options:
  --version VERSION  Install a release version (default: latest v* release)
  --prefix PATH      Installation root (default: $XDG_DATA_HOME/cagents-install)
  --stage-only       Stage a verified version without changing current, windows or daemon
  --restart-daemon   Checkpoint supported sessions and replace the running daemon
  --require-empty-daemon  With --restart-daemon, refuse to interrupt live sessions
  --relaunch-agents  With --restart-daemon, relaunch non-resumable agents into their resume picker
  --no-relaunch-agents  With --restart-daemon, refuse while agents are non-resumable
                     (without either, the installed CLI follows its Settings)
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
STAGE_ONLY=0
REQUIRE_EMPTY_DAEMON=0
RELAUNCH_AGENTS=
PREPARE_CANDIDATE=
PREPARED_CANDIDATE=

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
        --stage-only)
            STAGE_ONLY=1
            shift
            ;;
        --prepare-candidate|--prepared-candidate)
            [ "$#" -ge 2 ] || fail "$1 requires a job UUID"
            [ -n "$2" ] || fail "$1 requires a job UUID"
            if [ "$1" = --prepare-candidate ]; then PREPARE_CANDIDATE=$2; else PREPARED_CANDIDATE=$2; fi
            shift 2
            ;;
        --supports-prepared-candidate)
            [ "$#" -eq 1 ] || fail '--supports-prepared-candidate takes no arguments'
            exit 0
            ;;
        --restart-daemon)
            RESTART_DAEMON=1
            shift
            ;;
        --relaunch-agents)
            RELAUNCH_AGENTS=1
            shift
            ;;
        --no-relaunch-agents)
            RELAUNCH_AGENTS=0
            shift
            ;;
        --require-empty-daemon)
            REQUIRE_EMPTY_DAEMON=1
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

valid_candidate_job() {
    printf '%s\n' "$1" | LC_ALL=C awk -F- '
        NR != 1 || NF != 5 || length($1) != 8 || length($2) != 4 || length($3) != 4 ||
        length($4) != 4 || length($5) != 12 { exit 1 }
        { for (i = 1; i <= NF; i++) if ($i !~ /^[0-9a-f]+$/) exit 1 }
    '
}
if [ -n "$PREPARE_CANDIDATE$PREPARED_CANDIDATE" ]; then
    [ -z "$PREPARE_CANDIDATE" ] || [ -z "$PREPARED_CANDIDATE" ] \
        || fail 'candidate preparation and reuse cannot be combined'
    [ "$STAGE_ONLY" -eq 0 ] && [ "$UNINSTALL" -eq 0 ] \
        || fail 'private candidate preparation cannot be combined with staging or removal'
    candidate_job=${PREPARE_CANDIDATE:-$PREPARED_CANDIDATE}
    valid_candidate_job "$candidate_job" || fail 'candidate preparation requires a canonical job UUID'
    [ -n "${CAGENTS_UI_UPDATE_GUARD_FD:-}" ] \
        || fail 'private candidate preparation requires the worker application update fence'
fi

if [ "${CAGENTS_AGENT_AUTHORITY+x}" = x ]; then
    fail 'installation and removal require your own terminal; hosted sessions cannot change the installation. Cagents terminals are hosted sessions. A person must run this command from a system terminal outside cagents.'
fi

if [ "$STAGE_ONLY" -eq 1 ]; then
    [ "$RESTART_DAEMON" -eq 0 ] && [ "$UNINSTALL" -eq 0 ] \
        && [ "$REQUIRE_EMPTY_DAEMON" -eq 0 ] && [ -z "$RELAUNCH_AGENTS" ] \
        || fail '--stage-only cannot be combined with restart, relaunch, uninstall or purge options'
fi

[ -z "$RELAUNCH_AGENTS" ] || [ "$RESTART_DAEMON" -eq 1 ] \
    || fail '--relaunch-agents and --no-relaunch-agents require --restart-daemon'
[ "$REQUIRE_EMPTY_DAEMON" -eq 0 ] || [ "$RESTART_DAEMON" -eq 1 ] \
    || fail '--require-empty-daemon requires --restart-daemon'

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
if ! GLIBC_VERSION=$(getconf GNU_LIBC_VERSION 2>/dev/null); then
    fail 'the x86_64 glibc release requires a glibc system'
fi
case $GLIBC_VERSION in
    glibc\ *) ;;
    *) fail 'the x86_64 glibc release requires a glibc system' ;;
esac
if [ "$UNINSTALL" -eq 0 ]; then
    # GNU_LIBC_VERSION has the form "glibc MAJOR.MINOR". Validate it before
    # creating directories, downloading files, or installing an update. Removal
    # keeps the existing platform policy so older installations can be uninstalled.
    if printf '%s\n' "$GLIBC_VERSION" | LC_ALL=C awk '
        NR != 1 || $0 !~ /^glibc [0-9]+\.[0-9]+$/ { invalid = 1 }
        { split($2, version, ".") }
        END {
            if (invalid || NR != 1) exit 2
            exit !(version[1] > 2 || (version[1] == 2 && version[2] >= 35))
        }
    '; then
        :
    else
        case $? in
            1) fail "glibc 2.35 or newer is required (found $GLIBC_VERSION)" ;;
            *) fail 'could not determine a valid glibc version' ;;
        esac
    fi
fi

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
    if daemon_output=$(installer_tool "$daemon_cli" --json daemon status 2>/dev/null); then
        case $daemon_output in
            *'
'*) DAEMON_STATUS=unknown ;;
            *'"running":false'*'"daemon_present":true'*|*'"daemon_present":true'*'"running":false'*)
                # A real daemon can still be starting or draining. A held
                # reservation alone is never evidence of such a process.
                DAEMON_STATUS=unknown
                ;;
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

    DAEMON_SHUTDOWN_REQUESTED=1
    installer_tool "$daemon_cli" daemon stop --require-empty >/dev/null \
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
    if ! daemon_help=$(installer_tool "$daemon_cli" daemon --help 2>/dev/null) \
        || ! printf '%s\n' "$daemon_help" | awk '
            $1 == "prepare-upgrade" { supported = 1 }
            END { exit !supported }
        '; then
        fail 'the installed CLI cannot checkpoint upgrades; for the one-time legacy bootstrap, finish sessions or explicitly terminate disposable sessions with the installed CLI, then retry after its daemon exits; installation was not changed'
    fi
    set -- --yes
    if [ -n "$RELAUNCH_AGENTS" ]; then
        relaunch_help=$(installer_tool "$daemon_cli" daemon prepare-upgrade --help 2>/dev/null) \
            || fail 'cannot inspect installed upgrade support; installation was not changed'
        if [ "$RELAUNCH_AGENTS" -eq 1 ]; then
            printf '%s\n' "$relaunch_help" | awk '$1 == "--relaunch-agents" { supported = 1 } END { exit !supported }' \
                || fail 'the installed CLI cannot record agent relaunch acceptance; finish blocking sessions before upgrading; installation was not changed'
            set -- "$@" --relaunch-agents
        # A CLI older than the setting never relaunches unless asked to.
        elif printf '%s\n' "$relaunch_help" | awk '$1 == "--no-relaunch-agents" { supported = 1 } END { exit !supported }'; then
            set -- "$@" --no-relaunch-agents
        fi
    fi
    DAEMON_SHUTDOWN_REQUESTED=1
    installer_tool "$daemon_cli" --json daemon prepare-upgrade "$@" >/dev/null \
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
[ -z "$PREPARE_CANDIDATE" ] || need_command sync

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
NEW_VERSION_PREPARED=0
NEW_VERSION_EXISTING=0
NEW_VERSION_FRESH=0
NEW_VERSION_RETAINED=0
EXISTING_INSTALLED=0
CANDIDATE_MARKER_MISMATCH=0
PENDING_RECOVERY_INSPECTED=0
PREPARED_MARKER_TEMP=
CURRENT_TEMP=
CURRENT_SWITCHED=0
TRANSACTION_COMMITTED=0
HAD_CURRENT=0
PREVIOUS_CURRENT_TARGET=
ROLLBACK_OK=1
DAEMON_STOPPED_FOR_SWITCH=0
DAEMON_SHUTDOWN_REQUESTED=0
CENSUS_CAPTURED=0
RETAIN_RECOVERY=0
RECOVERY_OUTCOME_SETTLED=0
ORIGINAL_SERVICE_VERIFIED=0
INSTALL_CENSUS_PATH=${CAGENTS_INSTALL_CENSUS_PATH:-$DOWNLOAD_DIR/update-census.json}
INSTALL_OUTCOME_PATH=${CAGENTS_INSTALL_OUTCOME_PATH:-$DOWNLOAD_DIR/update-outcome.json}
DAEMON_CHECKPOINTED=0
OLD_DAEMON_WAS_RUNNING=0
DAEMON_STARTED=0
DAEMON_START_FAILED=0
PREUPDATE_NO_SOURCE=0
DAEMON_DEFERRED=0
DAEMON_START_ATTEMPTED=0
# The shared-model store's schema before the replacement daemon starts, which
# is the schema the previous daemon reads; `none` when there is no store yet.
STORE_SCHEMA_BEFORE=
# A store the replacement creates is converted back this far, the schema of
# releases before the converter.
OLDEST_STORE_SCHEMA=22
BIN_LINK=$HOME/.local/bin/cagents
DESKTOP_LINK=$DATA_HOME/applications/dev.cagents.Cagents.desktop
ICON_LINK=$DATA_HOME/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg
RASTER_ICON_LINK=$DATA_HOME/icons/hicolor/128x128/apps/dev.cagents.Cagents.png
METAINFO_LINK=$DATA_HOME/metainfo/dev.cagents.Cagents.metainfo.xml
AUTOSTART_LINK=$CONFIG_HOME/autostart/dev.cagents.Cagents.Daemon.desktop
# Each link points at the same path, relative to a version root, through current.
BIN_MEMBER=bin/cagents
DESKTOP_MEMBER=share/applications/dev.cagents.Cagents.desktop
ICON_MEMBER=share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg
RASTER_ICON_MEMBER=share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png
METAINFO_MEMBER=share/metainfo/dev.cagents.Cagents.metainfo.xml
AUTOSTART_MEMBER=share/autostart/dev.cagents.Cagents.Daemon.desktop
BIN_TARGET=$PREFIX/current/$BIN_MEMBER
DESKTOP_TARGET=$PREFIX/current/$DESKTOP_MEMBER
ICON_TARGET=$PREFIX/current/$ICON_MEMBER
RASTER_ICON_TARGET=$PREFIX/current/$RASTER_ICON_MEMBER
METAINFO_TARGET=$PREFIX/current/$METAINFO_MEMBER
AUTOSTART_TARGET=$PREFIX/current/$AUTOSTART_MEMBER
BIN_LINK_CREATED=0
DESKTOP_LINK_CREATED=0
ICON_LINK_CREATED=0
RASTER_ICON_LINK_CREATED=0
METAINFO_LINK_CREATED=0
AUTOSTART_LINK_CREATED=0
# Set just before an identical copy at a link path moves aside to its backup.
BIN_LINK_ADOPTED=0
DESKTOP_LINK_ADOPTED=0
ICON_LINK_ADOPTED=0
RASTER_ICON_LINK_ADOPTED=0
METAINFO_LINK_ADOPTED=0
AUTOSTART_LINK_ADOPTED=0
PREVIOUS_VERSION_DIR=

# An identical copy found at a link path is moved here, beside it, until the
# transaction commits, so a rollback can put the original file back.
link_backup_path() {
    printf '%s/.%s.cagents-install.%s\n' "${1%/*}" "${1##*/}" "$$"
}

# Restores or discards one adopted copy. The original returns only over this
# installer's own link or an empty path; anything else there is left alone.
settle_adopted_link() {
    settle_adopted=$1
    settle_link=$2
    settle_target=$3
    [ "$settle_adopted" -eq 1 ] || return 0
    settle_backup=$(link_backup_path "$settle_link")
    [ -e "$settle_backup" ] || [ -L "$settle_backup" ] || return 0
    if [ "$TRANSACTION_COMMITTED" -eq 1 ]; then
        rm -f "$settle_backup" || :
    elif { [ -L "$settle_link" ] && [ "$(readlink "$settle_link" 2>/dev/null || :)" = "$settle_target" ]; } \
        || { [ ! -e "$settle_link" ] && [ ! -L "$settle_link" ]; }; then
        if mv -fT "$settle_backup" "$settle_link"; then
            printf '%s: installation rolled back; restored the original file at %s\n' \
                "$PROGRAM" "$settle_link" >&2
        else
            printf '%s: could not restore %s; the original file is at %s\n' \
                "$PROGRAM" "$settle_link" "$settle_backup" >&2
        fi
    else
        printf '%s: %s changed during installation; the original file is at %s\n' \
            "$PROGRAM" "$settle_link" "$settle_backup" >&2
    fi
}
# These commands belong to the verified candidate, including after current
# rolls back to a release that cannot report recovery inventory itself.
verify_update_census() {
    set -- daemon verify-update-census --census "$INSTALL_CENSUS_PATH" \
        --outcome "$INSTALL_OUTCOME_PATH"
    [ "$STAGE_ONLY" -eq 0 ] || set -- "$@" --allow-absent
    installer_tool "$CANDIDATE_COORDINATOR" "$@" >/dev/null
}
verify_original_census() {
    # A different ready daemon can be valid recovery without being the source.
    # Only the unchanged, authenticated source permits rewinding its binaries.
    installer_tool "$CANDIDATE_COORDINATOR" daemon verify-update-census \
        --census "$INSTALL_CENSUS_PATH" --outcome "$INSTALL_OUTCOME_PATH" \
        --require-original >/dev/null 2>&1
}
verify_absent_census() {
    installer_tool "$CANDIDATE_COORDINATOR" daemon verify-update-census \
        --census "$INSTALL_CENSUS_PATH" --outcome "$INSTALL_OUTCOME_PATH" \
        --require-absent >/dev/null 2>&1
}
inspect_update_recovery() {
    installer_tool "$CANDIDATE_COORDINATOR" daemon inspect-update-recovery \
        --census "$INSTALL_CENSUS_PATH" --outcome "$INSTALL_OUTCOME_PATH" \
        --candidate-cli "$NEW_VERSION_DIR/bin/cagents" >/dev/null
}
mark_retained_candidate() {
    if [ "$NEW_VERSION_FRESH" -eq 1 ] && [ "$NEW_VERSION_PREPARED" -eq 0 ]; then
        # This private marker permits only a later archive/tree revalidation.
        # It grants neither recovery custody nor authority over a daemon.
        retained_temp=$NEW_VERSION_DIR/.retained-marker.$$
        if (umask 077; set -C; retained_marker_contents > "$retained_temp") \
            && mv -Tf "$retained_temp" "$NEW_VERSION_DIR/$RETAINED_MARKER"; then
            NEW_VERSION_RETAINED=1
        else
            rm -f "$retained_temp" || :
            printf '%s: could not record retained-candidate retry metadata; keep the candidate for inspection\n' "$PROGRAM" >&2
        fi
    fi
}
retain_recovery_evidence() {
    RETAIN_RECOVERY=1
    mark_retained_candidate
    recovery_executable=$(printf '%s' "$NEW_VERSION_DIR/bin/cagents" | sed "s/'/'\\\\''/g")
    printf "%s: terminal service readiness is unverified; retained the candidate and recovery evidence at %s and %s. Inspect with '%s' --no-start daemon status and '%s' --no-start terminal list.\n" \
        "$PROGRAM" "$NEW_VERSION_DIR" "$INSTALL_OUTCOME_PATH" "$recovery_executable" "$recovery_executable" >&2
}
print_candidate_start_command() {
    recovery_executable=$(printf '%s' "$NEW_VERSION_DIR/bin/cagents" | sed "s/'/'\\\\''/g")
    printf "%s: the selected daemon is not verified ready. Start it with '%s' daemon start, then inspect '%s' --no-start daemon status and '%s' --no-start terminal list.\n" \
        "$PROGRAM" "$recovery_executable" "$recovery_executable" "$recovery_executable" >&2
}
inspect_pending_update_recovery() {
    set -- daemon inspect-pending-update-recovery --candidate-cli "$NEW_VERSION_DIR/bin/cagents"
    if [ "$CENSUS_CAPTURED" -eq 1 ] || [ -n "${CAGENTS_INSTALL_CENSUS_PATH:-}" ]; then
        set -- "$@" --census "$INSTALL_CENSUS_PATH"
    fi
    installer_tool "$CANDIDATE_COORDINATOR" "$@" >/dev/null
}
cleanup() {
    cleanup_status=$?
    if [ -n "$INSTALL_STAGE" ]; then
        rm -rf "$INSTALL_STAGE" || :
    fi
    if [ -n "$CURRENT_TEMP" ]; then
        rm -f "$CURRENT_TEMP" || :
    fi
    if [ -n "$PREPARED_MARKER_TEMP" ]; then
        rm -f "$PREPARED_MARKER_TEMP" || :
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] && [ -n "$NEW_VERSION_DIR" ] \
        && [ "$CENSUS_CAPTURED" -eq 1 ]; then
        read_daemon_status "$NEW_VERSION_DIR/bin/cagents"
        if [ "$DAEMON_STATUS" != stopped ]; then
            if verify_original_census; then
                ORIGINAL_SERVICE_VERIFIED=1
            else
                # A recovery writer can start before this shell's start call,
                # including during launcher publication. Keep its installation.
                TRANSACTION_COMMITTED=1
                RETAIN_RECOVERY=1
                mark_retained_candidate
                if ! verify_update_census; then
                    # Unknown status can conceal Prepared recovery. Preserve
                    # its exact shared-owner command instead of only status.
                    inspect_update_recovery || :
                    retain_recovery_evidence
                fi
                RECOVERY_OUTCOME_SETTLED=1
                printf '%s: the original terminal service could not be verified; retained the candidate and recovery evidence instead of rewinding its binaries\n' \
                    "$PROGRAM" >&2
            fi
        fi
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] && [ "$CURRENT_SWITCHED" -eq 1 ] \
        && [ "$ORIGINAL_SERVICE_VERIFIED" -eq 0 ] \
        && { [ "$RESTART_DAEMON" -eq 1 ] || [ "$DAEMON_SHUTDOWN_REQUESTED" -eq 1 ]; }; then
        # Keep a capable target selected while a Prepared checkpoint blocks
        # rollback. Repointing current first could strand an older source.
        if ! inspect_update_recovery; then
            TRANSACTION_COMMITTED=1
            retain_recovery_evidence
            RECOVERY_OUTCOME_SETTLED=1
            cleanup_status=1
        fi
    fi

    # Any daemon of the new version that started after current switched, the
    # installer's or one a launcher started, may have converted the
    # shared-model store past what the restored daemon reads. The replacement
    # converts it back to the schema recorded before the switch; that does
    # nothing if the store was not converted, and refuses while a daemon runs.
    # The previous release is never run to ask, since releases before the
    # converter take no options and would start a daemon instead. Without the
    # conversion the restored daemon could not open the store, so the new
    # installation is retained instead.
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] && [ "$CURRENT_SWITCHED" -eq 1 ] \
        && [ -n "$STORE_SCHEMA_BEFORE" ]; then
        restored_schema=$STORE_SCHEMA_BEFORE
        [ "$restored_schema" != none ] || restored_schema=$OLDEST_STORE_SCHEMA
        if ! installer_lease_tool "$NEW_VERSION_DIR/bin/cagents" daemon downgrade-store "$restored_schema" >/dev/null; then
            TRANSACTION_COMMITTED=1
            RETAIN_RECOVERY=1
            mark_retained_candidate
            printf '%s: could not convert the shared-model store back to schema %s, which the previous version reads; retained the new installation, which reads the store\n' \
                "$PROGRAM" "$restored_schema" >&2
            if inspect_update_recovery; then
                if [ -n "${CAGENTS_DAEMON_LEASE_FD:-}" ]; then
                    retain_recovery_evidence
                    print_candidate_start_command
                else
                    candidate_start_completed=0
                    if installer_tool "$NEW_VERSION_DIR/bin/cagents" daemon start >/dev/null; then
                        candidate_start_completed=1
                    fi
                    if ! verify_update_census; then
                        # Either exit status can leave a serving or starting
                        # process. Preserve it and the failed census receipt.
                        read_daemon_status "$NEW_VERSION_DIR/bin/cagents"
                        candidate_after_start=$DAEMON_STATUS
                        pending_after_start=1
                        if inspect_pending_update_recovery; then pending_after_start=0; fi
                        retain_recovery_evidence
                        if [ "$candidate_start_completed" -eq 1 ]; then
                            printf '%s: the daemon start command completed, but readiness and the original inventory remain unverified; keep any serving daemon running and use the inspection commands above\n' \
                                "$PROGRAM" >&2
                        elif [ "$candidate_after_start" = stopped ] && [ "$pending_after_start" -eq 0 ] \
                            && inspect_update_recovery; then
                            print_candidate_start_command
                        else
                            printf '%s: the daemon start command failed, but a stopped service was not proven; keep any serving daemon running and use the inspection commands above\n' \
                                "$PROGRAM" >&2
                        fi
                    fi
                fi
                RECOVERY_OUTCOME_SETTLED=1
            else
                retain_recovery_evidence
                RECOVERY_OUTCOME_SETTLED=1
            fi
        fi
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] && [ "$CURRENT_SWITCHED" -eq 1 ]; then
        if [ ! -L "$PREFIX/current" ] || [ "$(readlink "$PREFIX/current" 2>/dev/null || :)" != "versions/$VERSION" ]; then
            ROLLBACK_OK=0
            printf '%s: current changed independently; retained both version trees for recovery at %s\n' "$PROGRAM" "$PREFIX" >&2
        elif [ "$HAD_CURRENT" -eq 1 ]; then
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
        && [ "$DAEMON_SHUTDOWN_REQUESTED" -eq 1 ] \
        && [ "$OLD_DAEMON_WAS_RUNNING" -eq 1 ] \
        && [ -z "${CAGENTS_DAEMON_LEASE_FD:-}" ] \
        && [ "$ROLLBACK_OK" -eq 1 ]; then
        # Prepared checkpoints can require the target's recovery implementation.
        # A successful old startup is never proof that its terminals returned.
        read_daemon_status "$PREFIX/current/bin/cagents"
        if [ "$DAEMON_STATUS" = running ]; then
            if ! verify_update_census; then
                retain_recovery_evidence
                cleanup_status=1
            fi
        elif inspect_update_recovery; then
            if installer_tool "$PREFIX/current/bin/cagents" daemon start >/dev/null \
                && verify_update_census; then
                :
            else
                retain_recovery_evidence
                cleanup_status=1
            fi
        else
            retain_recovery_evidence
            cleanup_status=1
        fi
    elif [ "$cleanup_status" -ne 0 ] && [ "$CENSUS_CAPTURED" -eq 1 ] \
        && [ "$RECOVERY_OUTCOME_SETTLED" -eq 0 ]; then
        # The worker owns restart while it retains the handoff reservation.
        # Report typed evidence about current if serving, otherwise the
        # checkpoint's required next owner; never borrow a lock as readiness.
        read_daemon_status "$PREFIX/current/bin/cagents"
        if [ "$DAEMON_STATUS" = running ]; then
            if ! verify_update_census; then retain_recovery_evidence; fi
        else
            inspect_update_recovery || :
            # No Prepared blocker only permits a future start. It does not
            # establish readiness or prove that any terminals are restored.
            retain_recovery_evidence
        fi
    fi

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$BIN_LINK_CREATED" -eq 1 ] && [ -L "$BIN_LINK" ] \
        && [ "$(readlink "$BIN_LINK" 2>/dev/null || :)" = "$BIN_TARGET" ]; then
        rm -f "$BIN_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$DESKTOP_LINK_CREATED" -eq 1 ] && [ -L "$DESKTOP_LINK" ] \
        && [ "$(readlink "$DESKTOP_LINK" 2>/dev/null || :)" = "$DESKTOP_TARGET" ]; then
        rm -f "$DESKTOP_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$ICON_LINK_CREATED" -eq 1 ] && [ -L "$ICON_LINK" ] \
        && [ "$(readlink "$ICON_LINK" 2>/dev/null || :)" = "$ICON_TARGET" ]; then
        rm -f "$ICON_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$RASTER_ICON_LINK_CREATED" -eq 1 ] && [ -L "$RASTER_ICON_LINK" ] \
        && [ "$(readlink "$RASTER_ICON_LINK" 2>/dev/null || :)" = "$RASTER_ICON_TARGET" ]; then
        rm -f "$RASTER_ICON_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$METAINFO_LINK_CREATED" -eq 1 ] && [ -L "$METAINFO_LINK" ] \
        && [ "$(readlink "$METAINFO_LINK" 2>/dev/null || :)" = "$METAINFO_TARGET" ]; then
        rm -f "$METAINFO_LINK" || :
    fi
    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$AUTOSTART_LINK_CREATED" -eq 1 ] && [ -L "$AUTOSTART_LINK" ] \
        && [ "$(readlink "$AUTOSTART_LINK" 2>/dev/null || :)" = "$AUTOSTART_TARGET" ]; then
        rm -f "$AUTOSTART_LINK" || :
    fi
    settle_adopted_link "$BIN_LINK_ADOPTED" "$BIN_LINK" "$BIN_TARGET"
    settle_adopted_link "$DESKTOP_LINK_ADOPTED" "$DESKTOP_LINK" "$DESKTOP_TARGET"
    settle_adopted_link "$ICON_LINK_ADOPTED" "$ICON_LINK" "$ICON_TARGET"
    settle_adopted_link "$RASTER_ICON_LINK_ADOPTED" "$RASTER_ICON_LINK" "$RASTER_ICON_TARGET"
    settle_adopted_link "$METAINFO_LINK_ADOPTED" "$METAINFO_LINK" "$METAINFO_TARGET"
    settle_adopted_link "$AUTOSTART_LINK_ADOPTED" "$AUTOSTART_LINK" "$AUTOSTART_TARGET"

    if [ "$TRANSACTION_COMMITTED" -eq 0 ] \
        && [ -n "$NEW_VERSION_DIR" ] && [ "$ROLLBACK_OK" -eq 1 ] \
        && [ "$NEW_VERSION_PREPARED" -eq 0 ] \
        && [ "$NEW_VERSION_EXISTING" -eq 0 ] \
        && [ "$RETAIN_RECOVERY" -eq 0 ]; then
        rm -rf "$NEW_VERSION_DIR" || :
    fi
    if [ "$RETAIN_RECOVERY" -eq 0 ]; then
        rm -rf "$DOWNLOAD_DIR" || :
    else
        # Preserve the small census/receipt evidence. The installed candidate
        # remains available for inspection; do not strand large archive and
        # extracted executable copies in a possibly RAM-backed TMPDIR.
        rm -f "$ARCHIVE_PATH" "$CHECKSUMS_PATH" \
            "$CANDIDATE_COORDINATOR" "$CANDIDATE_INSPECTOR" || :
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

command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 \
    || fail 'sha256sum or shasum is required'
file_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print tolower($1) }'
    else
        shasum -a 256 "$1" | awk '{ print tolower($1) }'
    fi
}
ACTUAL_HASH=$(file_sha256 "$ARCHIVE_PATH")
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
    $0 == top "/share/autostart/dev.cagents.Cagents.Daemon.desktop" { required[11] = 1 }
    $0 == top "/share/doc/cagents/LICENSE" { required[8] = 1 }
    END {
        for (i = 1; i <= 11; i++) if (!required[i]) exit 1
    }
' "$ARCHIVE_LIST"; then
    fail 'archive has an unexpected or incomplete entry structure'
fi

# The verified candidate owns the UI launch fence before any installation
# mutation. Re-entry reuses these downloaded assets and is authorized only by
# the actual inherited lock, never by this environment marker alone.
CANDIDATE_COORDINATOR=$DOWNLOAD_DIR/cagents-install-coordinator
tar -xOzf "$ARCHIVE_PATH" -- "$TOP_LEVEL/bin/cagents" > "$CANDIDATE_COORDINATOR" \
    || fail 'could not extract the verified update coordinator'
# Source-pane preflight runs the matched host beside the selected coordinator.
# A fresh install (or an old CLI without the guard) uses this extracted pair.
CANDIDATE_INSPECTOR=$DOWNLOAD_DIR/cagentsd
tar -xOzf "$ARCHIVE_PATH" -- "$TOP_LEVEL/bin/cagentsd" > "$CANDIDATE_INSPECTOR" \
    || fail 'could not extract the verified source inspector'
chmod 0700 "$CANDIDATE_COORDINATOR" "$CANDIDATE_INSPECTOR"
supports_install_guard() {
    [ -x "$1" ] \
        && installer_tool "$1" coordinate-install --help >/dev/null 2>&1 \
        && installer_tool "$1" verify-install-guard --help >/dev/null 2>&1 \
        && installer_tool "$1" record-deferred-install --help >/dev/null 2>&1
}
supports_stage_guard() {
    supports_install_guard "$1" || return 1
    for stage_command in coordinate-install verify-install-guard; do
        stage_help=$(installer_tool "$1" "$stage_command" --help 2>/dev/null) || return 1
        printf '%s\n' "$stage_help" | awk '$1 == "--stage-only" { found = 1 } END { exit !found }' || return 1
    done
}
# The replacement's own command line reports whether it can read and convert
# the shared-model store, without starting anything.
supports_store_conversion() {
    [ -x "$1" ] \
        && installer_tool "$1" daemon store-schema --help >/dev/null 2>&1 \
        && installer_tool "$1" daemon downgrade-store --help >/dev/null 2>&1
}
supports_selected_guard() {
    if [ "$STAGE_ONLY" -eq 1 ]; then supports_stage_guard "$1"; else supports_install_guard "$1"; fi
}
if [ -n "$PREPARE_CANDIDATE$PREPARED_CANDIDATE" ]; then
    if [ -n "$PREPARE_CANDIDATE" ]; then
        supports_stage_guard "$CANDIDATE_COORDINATOR"
    else
        supports_install_guard "$CANDIDATE_COORDINATOR"
    fi \
        || fail 'the candidate cannot validate the worker application update fence'
    COORDINATOR=$CANDIDATE_COORDINATOR
elif [ "$STAGE_ONLY" -eq 1 ]; then
    # Staged launch routing is interpreted by this exact candidate set. An old
    # candidate cannot borrow a newer installed coordinator's staging support.
    supports_stage_guard "$CANDIDATE_COORDINATOR" \
        || fail 'this release does not support safe staging; installation was not changed'
    COORDINATOR=$CANDIDATE_COORDINATOR
elif [ -e "$PREFIX/versions/$VERSION" ] || [ -L "$PREFIX/versions/$VERSION" ]; then
    # The selected version may be an unvalidated retained tree, including
    # current itself. Admission/probing executes only the authenticated archive.
    supports_install_guard "$CANDIDATE_COORDINATOR" \
        || fail 'the verified archive cannot inspect this retained installation; the existing version was preserved'
    COORDINATOR=$CANDIDATE_COORDINATOR
elif supports_selected_guard "$PREFIX/current/bin/cagents"; then
    # A newer installed coordinator also makes rollback to an older release
    # safe when that candidate predates the window-coordination commands.
    COORDINATOR=$PREFIX/current/bin/cagents
elif supports_selected_guard "$CANDIDATE_COORDINATOR"; then
    COORDINATOR=$CANDIDATE_COORDINATOR
else
    fail 'neither the installed CLI nor this release supports safe window coordination; install a current release first, then retry this version; installation was not changed'
fi
if [ -n "${CAGENTS_UI_UPDATE_GUARD_FD:-}" ]; then
    set -- verify-install-guard
    if [ "$STAGE_ONLY" -eq 1 ] || [ -n "$PREPARE_CANDIDATE" ]; then
        set -- "$@" --stage-only
    fi
    installer_tool "$COORDINATOR" "$@" >/dev/null \
        || fail 'inherited application update fence is invalid; installation was not changed'
else
    INSTALLER_SOURCE=$0
    if [ ! -f "$INSTALLER_SOURCE" ] || [ ! -r "$INSTALLER_SOURCE" ]; then
        # A stdin invocation has no script to re-enter. Only the same selected
        # release's verified installer may substitute for it; local archives
        # never cause an unexpected network fetch.
        [ -z "$LOCAL_ARCHIVE" ] \
            || fail 'save install.sh to a file and run sh /path/to/install.sh with the same options; stdin cannot be coordinated with a local archive'
        INSTALLER_SOURCE=$DOWNLOAD_DIR/install.sh
        download_release_asset install.sh "$INSTALLER_SOURCE"
        INSTALLER_HASH=$(awk '
            $2 == "install.sh" { count++; hash = tolower($1) }
            END { if (count != 1) exit 1; print hash }
        ' "$CHECKSUMS_PATH") || fail 'SHA256SUMS does not identify this release installer'
        INSTALLER_ACTUAL=$(file_sha256 "$INSTALLER_SOURCE")
        [ "$INSTALLER_ACTUAL" = "$INSTALLER_HASH" ] || fail 'installer SHA-256 verification failed'
    fi
    set -- --version "$VERSION" --prefix "$PREFIX"
    [ "$STAGE_ONLY" -eq 0 ] || set -- "$@" --stage-only
    [ "$RESTART_DAEMON" -eq 0 ] || set -- "$@" --restart-daemon
    [ "$REQUIRE_EMPTY_DAEMON" -eq 0 ] || set -- "$@" --require-empty-daemon
    case $RELAUNCH_AGENTS in
        1) set -- "$@" --relaunch-agents ;;
        0) set -- "$@" --no-relaunch-agents ;;
    esac
    # Keep the downloaded assets until the coordinator finishes cancellation
    # and window relaunch. A signal interrupts wait, so retry it after forwarding.
    COORDINATOR_PID=
    COORDINATOR_INTERRUPTED=0
    trap 'COORDINATOR_INTERRUPTED=1; if [ -n "$COORDINATOR_PID" ]; then kill -TERM "$COORDINATOR_PID" 2>/dev/null || :; fi' HUP INT TERM
    if [ "$STAGE_ONLY" -eq 1 ]; then
        set -- --stage-only --prefix "$PREFIX" --installer "$INSTALLER_SOURCE" -- "$@"
    else
        set -- --prefix "$PREFIX" --installer "$INSTALLER_SOURCE" -- "$@"
    fi
    # The background PID must be the coordinator itself. Calling a function
    # whose body is a subshell would leave an intermediate shell as $!, so
    # cancellation could kill the waiter while its installer kept running.
    (
        if [ "${CAGENTS_DAEMON_LEASE_FD:-}" = 3 ]; then exec 3<&-; fi
        unset CAGENTS_DAEMON_LEASE_FD
        installer_drop_recovery
        export CAGENTS_ARCHIVE=$ARCHIVE_PATH CAGENTS_CHECKSUMS=$CHECKSUMS_PATH
        exec "$COORDINATOR" coordinate-install "$@"
    ) &
    COORDINATOR_PID=$!
    if [ "$COORDINATOR_INTERRUPTED" -eq 1 ]; then
        kill -TERM "$COORDINATOR_PID" 2>/dev/null || :
    fi
    while :; do
        COORDINATOR_INTERRUPTED=0
        if wait "$COORDINATOR_PID"; then COORDINATOR_STATUS=0; else COORDINATOR_STATUS=$?; fi
        [ "$COORDINATOR_INTERRUPTED" -eq 1 ] || break
    done
    COORDINATOR_PID=
    trap 'exit 1' HUP INT TERM
    exit "$COORDINATOR_STATUS"
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
PREPARED_MARKER=.cagents-prepared-candidate-v1.json
RETAINED_MARKER=.cagents-retained-candidate-v1.json
prepared_marker_contents() {
    printf '{"version":1,"job":"%s","archive_sha256":"%s"}\n' "${1:-$candidate_job}" "$ACTUAL_HASH"
}
retained_marker_contents() {
    printf '{"version":1,"archive_sha256":"%s"}\n' "$ACTUAL_HASH"
}
if [ -n "$PREPARED_CANDIDATE" ] || [ -e "$PREFIX/versions/$VERSION" ] || [ -L "$PREFIX/versions/$VERSION" ]; then
    prepared_root=$PREFIX/versions/$VERSION
    [ -d "$prepared_root" ] && [ ! -L "$prepared_root" ] \
        || fail 'the explicitly prepared candidate is missing or unsafe'
    NEW_VERSION_DIR=$prepared_root
    NEW_VERSION_EXISTING=1
    if [ -z "$PREPARE_CANDIDATE" ] && [ -z "${CAGENTS_DAEMON_LEASE_FD:-}" ]; then
        # Only the freshly authenticated archive coordinator executes here.
        # The existing target is path data for shared recovery inspection;
        # its markers or extra files cannot hide retained Prepared recovery.
        if ! inspect_pending_update_recovery; then
            RETAIN_RECOVERY=1
            fail 'retained recovery requires the explicit command above; the candidate and original checkpoint were preserved'
        fi
        PENDING_RECOVERY_INSPECTED=1
    fi
    prepared_marker=$prepared_root/$PREPARED_MARKER
    if [ -e "$prepared_marker" ] || [ -L "$prepared_marker" ] || [ -n "$PREPARED_CANDIDATE" ]; then
        [ -f "$prepared_marker" ] && [ ! -L "$prepared_marker" ] \
            && [ "$(find "$prepared_marker" -maxdepth 0 -type f -uid "$(id -u)" -perm 0600 -print)" = "$prepared_marker" ] \
            || fail 'the prepared candidate does not match this private job and verified archive'
        if [ -z "$PREPARED_CANDIDATE" ]; then
            # A new guarded job can adopt an unchanged, privately marked
            # candidate. This grants no source or recovery authority.
            previous_candidate_job=$(sed -n 's/^{"version":1,"job":"\([0-9a-f-]*\)","archive_sha256":"[0-9a-f]*"}$/\1/p' "$prepared_marker")
            valid_candidate_job "$previous_candidate_job" \
                || fail "the previous prepared candidate marker is invalid; inspect '$prepared_root/bin/cagents' --no-start daemon status"
        else
            previous_candidate_job=$candidate_job
        fi
        if [ "$(cat "$prepared_marker")" != "$(prepared_marker_contents "$previous_candidate_job")" ]; then
            [ -z "$PREPARE_CANDIDATE$PREPARED_CANDIDATE" ] \
                || fail 'the prepared candidate does not match this private job and verified archive'
            CANDIDATE_MARKER_MISMATCH=1
        fi
        NEW_VERSION_PREPARED=1
    fi
    retained_marker=$prepared_root/$RETAINED_MARKER
    if [ -e "$retained_marker" ] || [ -L "$retained_marker" ]; then
        [ -f "$retained_marker" ] && [ ! -L "$retained_marker" ] \
            && [ "$(find "$retained_marker" -maxdepth 0 -type f -uid "$(id -u)" -perm 0600 -print)" = "$retained_marker" ] \
            || fail 'the retained candidate does not match this verified archive'
        if [ "$(cat "$retained_marker")" != "$(retained_marker_contents)" ]; then
            [ -z "$PREPARE_CANDIDATE$PREPARED_CANDIDATE" ] \
                || fail 'the retained candidate does not match this verified archive'
            CANDIDATE_MARKER_MISMATCH=1
        fi
        NEW_VERSION_RETAINED=1
    fi
    if [ "$NEW_VERSION_PREPARED" -eq 0 ] && [ "$NEW_VERSION_RETAINED" -eq 0 ]; then
        [ -z "$PREPARE_CANDIDATE$PREPARED_CANDIDATE" ] \
            || fail 'the prepared candidate does not match this private job and verified archive'
        EXISTING_INSTALLED=1
    fi
    NEW_VERSION_DIR=$prepared_root
    NEW_VERSION_EXISTING=1
fi
if [ -z "$PREPARE_CANDIDATE" ]; then
    if [ -e "$PREFIX/current" ] && [ ! -L "$PREFIX/current" ]; then
        fail "refusing to replace non-symlink $PREFIX/current"
    fi

    if [ -L "$PREFIX/current" ]; then
        PREVIOUS_CURRENT_TARGET=$(readlink "$PREFIX/current") \
            || fail "cannot inspect $PREFIX/current"
        HAD_CURRENT=1
        case $PREVIOUS_CURRENT_TARGET in
            /*) PREVIOUS_VERSION_DIR=$PREVIOUS_CURRENT_TARGET ;;
            *) PREVIOUS_VERSION_DIR=$PREFIX/$PREVIOUS_CURRENT_TARGET ;;
        esac
    fi
fi

if [ "$STAGE_ONLY" -eq 1 ]; then
    [ "$HAD_CURRENT" -eq 1 ] && [ -d "$PREVIOUS_VERSION_DIR" ] \
        || fail '--stage-only requires an existing installation; run install.sh without --stage-only for the first install'
fi

# Succeeds when COPY is a regular file with the same bytes as MEMBER in any of
# the given version roots. Desktop tools can replace a link with a copy of its
# target; such a copy carries no change of its own.
is_managed_copy() {
    managed_copy=$1
    managed_member=$2
    shift 2
    [ -f "$managed_copy" ] && [ ! -L "$managed_copy" ] || return 1
    managed_copy_hash=$(file_sha256 "$managed_copy")
    [ -n "$managed_copy_hash" ] || return 1
    for managed_root do
        [ -n "$managed_root" ] && [ -f "$managed_root/$managed_member" ] || continue
        [ "$(file_sha256 "$managed_root/$managed_member")" = "$managed_copy_hash" ] && return 0
    done
    return 1
}

# Refuses LINK unless it is absent, this installer's own symlink to MEMBER
# through current, or a managed copy of MEMBER from one of the version roots.
check_link_destination() {
    link=$1
    link_member=$2
    shift 2
    if [ -L "$link" ]; then
        target=$(readlink "$link") || fail "cannot inspect $link"
        [ "$target" = "$PREFIX/current/$link_member" ] \
            || fail "refusing to replace unowned symlink $link"
    elif [ -e "$link" ]; then
        is_managed_copy "$link" "$link_member" "$@" \
            || fail "refusing to replace existing file $link"
    fi
}

prepare_link_parent() {
    link_directory=${1%/*}
    mkdir -p "$link_directory" || fail "cannot create link directory $link_directory"
    [ -d "$link_directory" ] || fail "link parent is not a directory: $link_directory"
}

desktop_exec_escape() {
    desktop_exec_quoted=$(printf '%s' "$1" | sed \
        -e 's/\\/\\\\/g' \
        -e 's/"/\\"/g' \
        -e 's/`/\\`/g' \
        -e 's/\$/\\$/g' \
        -e 's/%/%%/g')
    # Desktop-entry string unescaping happens before Exec argument parsing.
    desktop_string_escape "$desktop_exec_quoted"
}

desktop_string_escape() {
    printf '%s\n' "$1" | sed \
        -e ':join' -e '$!{N;b join;}' \
        -e 's/\\/\\\\/g' \
        -e 's/\t/\\t/g' \
        -e 's/\r/\\r/g' \
        -e 's/\n/\\n/g'
}

INSTALL_STAGE=$PREFIX/.staging.$$
(umask 077 && mkdir "$INSTALL_STAGE") || fail 'could not create installation staging directory'
tar --no-same-owner --no-same-permissions -xzf "$ARCHIVE_PATH" -C "$INSTALL_STAGE" \
    || fail 'archive extraction failed'
AUTOSTART_EXEC=$(desktop_exec_escape "$BIN_TARGET")
DESKTOP_EXEC=$(desktop_exec_escape "$BIN_TARGET")
# GLib checks the executable exists before expanding %% in Exec. Keep paths
# containing percent signs out of that lookup and only in positional arguments.
DESKTOP_EXEC_PREFIX=
case $BIN_TARGET in
    *%*) DESKTOP_EXEC_PREFIX='/bin/sh -c "exec \\"\\$@\\"" cagents-desktop ' ;;
esac
DESKTOP_TRY_EXEC=$(desktop_string_escape "$BIN_TARGET")
DESKTOP_ICON=$(desktop_string_escape "$PREFIX/current/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg")
DESKTOP_FILE=$INSTALL_STAGE/$TOP_LEVEL/share/applications/dev.cagents.Cagents.desktop
{
    while IFS= read -r line || [ -n "$line" ]; do
        case $line in
            Exec=*) printf 'Exec=%s"%s" ui\n' "$DESKTOP_EXEC_PREFIX" "$DESKTOP_EXEC" ;;
            TryExec=*) printf 'TryExec=%s\n' "$DESKTOP_TRY_EXEC" ;;
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
    printf 'Exec=%s"%s" daemon start\n' "$DESKTOP_EXEC_PREFIX" "$AUTOSTART_EXEC"
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

if [ "$NEW_VERSION_EXISTING" -eq 1 ]; then
    # Re-extract the authenticated archive and repeat desktop normalization.
    # A private marker alone is not evidence that its executable tree survived.
    find "$INSTALL_STAGE/$TOP_LEVEL" -printf '%P\n' > "$DOWNLOAD_DIR/expected-tree"
    while IFS= read -r prepared_member; do
        expected_member=$INSTALL_STAGE/$TOP_LEVEL/$prepared_member
        actual_member=$NEW_VERSION_DIR/$prepared_member
        expected_kind=$(find "$expected_member" -maxdepth 0 -printf '%y:%m')
        actual_kind=$(find "$actual_member" -maxdepth 0 -printf '%y:%m') \
            || fail 'the prepared candidate is incomplete'
        [ "$expected_kind" = "$actual_kind" ] \
            || fail 'the prepared candidate file types or permissions changed'
        case $expected_kind in
            f:*) expected_member_hash=$(file_sha256 "$expected_member")
                [ -n "$expected_member_hash" ] \
                && [ "$expected_member_hash" = "$(file_sha256 "$actual_member")" ] \
                || fail 'the prepared candidate contents changed' ;;
        esac
    done < "$DOWNLOAD_DIR/expected-tree"
    # Pass untrusted existing names as argv, not lines: a newline in an extra
    # name must not split it into two apparently valid archive members.
    find "$NEW_VERSION_DIR" -mindepth 1 -exec sh -c '
        prepared_root=$1 expected_root=$2 marker=$3 retained=$4
        shift 4
        for actual do
            member=${actual#"$prepared_root/"}
            { [ "$member" = "$marker" ] || [ "$member" = "$retained" ]; } && continue
            [ -e "$expected_root/$member" ] || exit 1
        done
    ' cagents-prepared-tree "$NEW_VERSION_DIR" "$INSTALL_STAGE/$TOP_LEVEL" \
        "$( [ "$NEW_VERSION_PREPARED" -eq 0 ] || printf '%s' "$PREPARED_MARKER" )" \
        "$( [ "$NEW_VERSION_RETAINED" -eq 0 ] || printf '%s' "$RETAINED_MARKER" )" {} + \
        || fail 'the prepared candidate contains unexpected entries'
    rm -rf "$INSTALL_STAGE"
else
    if [ -n "$PREPARE_CANDIDATE" ]; then
        prepared_marker_contents > "$INSTALL_STAGE/$TOP_LEVEL/$PREPARED_MARKER"
        chmod 0600 "$INSTALL_STAGE/$TOP_LEVEL/$PREPARED_MARKER"
    fi
    NEW_VERSION_DIR=$PREFIX/versions/$VERSION
    mv -T "$INSTALL_STAGE/$TOP_LEVEL" "$NEW_VERSION_DIR"
    rmdir "$INSTALL_STAGE"
    NEW_VERSION_FRESH=1
fi
INSTALL_STAGE=
if [ -n "$PREPARE_CANDIDATE" ]; then
    if [ "$NEW_VERSION_EXISTING" -eq 1 ]; then
        PREPARED_MARKER_TEMP=$NEW_VERSION_DIR/.prepared-marker.$$
        (set -C; prepared_marker_contents > "$PREPARED_MARKER_TEMP") \
            || fail 'cannot prepare the new private candidate marker'
        chmod 0600 "$PREPARED_MARKER_TEMP"
        mv -Tf "$PREPARED_MARKER_TEMP" "$NEW_VERSION_DIR/$PREPARED_MARKER" \
            || fail 'cannot publish the new private candidate marker'
        PREPARED_MARKER_TEMP=
    fi
    # GNU sync -f performs syncfs: flush the full extracted tree, its private
    # marker and the atomic publication's parent before shutdown is permitted.
    sync -f "$PREFIX/versions" || fail 'could not durably publish the recovery candidate; the source service was not changed'
    NEW_VERSION_PREPARED=1
    TRANSACTION_COMMITTED=1
    printf 'Prepared cagents %s candidate at %s; running services and launchers were preserved.\n' \
        "$VERSION" "$NEW_VERSION_DIR"
    exit 0
fi
if [ "$STAGE_ONLY" -eq 1 ]; then
    # Publish the authenticated private tree; current and all public links
    # continue to select the serving build until the worker proves success.
    TRANSACTION_COMMITTED=1
    printf 'Staged cagents %s; current, running windows and daemon were preserved. Run "%s/bin/cagents" daemon upgrade to apply it when ready.\n' "$VERSION" "$NEW_VERSION_DIR"
    exit 0
fi

# Before rejecting an already published version, expose any retained Prepared
# recovery through the shared owner. Never replace its original census with a
# fresh empty capture, and never call an existing version "success" by itself.
if [ "$PENDING_RECOVERY_INSPECTED" -eq 0 ] && [ -z "${CAGENTS_DAEMON_LEASE_FD:-}" ]; then
    if ! inspect_pending_update_recovery; then
        RETAIN_RECOVERY=1
        mark_retained_candidate
        fail 'retained recovery requires the explicit command above; the candidate and original checkpoint were preserved'
    fi
fi
[ "$CANDIDATE_MARKER_MISMATCH" -eq 0 ] \
    || fail "the retained candidate archive differs; the existing tree was preserved. Inspect '$NEW_VERSION_DIR/bin/cagents' --no-start daemon status"
[ "$EXISTING_INSTALLED" -eq 0 ] || fail "version $VERSION is already installed at $PREFIX; inspect '$NEW_VERSION_DIR/bin/cagents' --no-start daemon status before retrying"

# A copy at a link path is compared with the finished candidate files, so the
# rewritten desktop and autostart entries count, and with the current version.
check_link_destination "$BIN_LINK" "$BIN_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
check_link_destination "$DESKTOP_LINK" "$DESKTOP_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
check_link_destination "$ICON_LINK" "$ICON_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
check_link_destination "$RASTER_ICON_LINK" "$RASTER_ICON_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
check_link_destination "$METAINFO_LINK" "$METAINFO_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
check_link_destination "$AUTOSTART_LINK" "$AUTOSTART_MEMBER" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR"
prepare_link_parent "$BIN_LINK"
prepare_link_parent "$DESKTOP_LINK"
prepare_link_parent "$ICON_LINK"
prepare_link_parent "$RASTER_ICON_LINK"
prepare_link_parent "$METAINFO_LINK"
prepare_link_parent "$AUTOSTART_LINK"

# Replaces a managed copy at LINK with the link. The copy moves aside before
# it is compared, so the bytes compared are the bytes replaced, and a file that
# appears at LINK meanwhile (even a directory, hence -T) makes ln fail instead
# of being overwritten.
adopt_managed_copy() {
    adopt_member=$1
    adopt_link=$2
    adopt_kind=$3
    adopt_backup=$(link_backup_path "$adopt_link")
    [ ! -e "$adopt_backup" ] && [ ! -L "$adopt_backup" ] \
        || fail "refusing to replace existing file $adopt_link while $adopt_backup exists"
    case $adopt_kind in
        bin) BIN_LINK_ADOPTED=1 ;;
        desktop) DESKTOP_LINK_ADOPTED=1 ;;
        icon) ICON_LINK_ADOPTED=1 ;;
        raster_icon) RASTER_ICON_LINK_ADOPTED=1 ;;
        metainfo) METAINFO_LINK_ADOPTED=1 ;;
        autostart) AUTOSTART_LINK_ADOPTED=1 ;;
        *) fail 'internal error: unknown link kind' ;;
    esac
    mv -T "$adopt_link" "$adopt_backup" || fail "cannot move $adopt_link aside to replace it"
    is_managed_copy "$adopt_backup" "$adopt_member" "$NEW_VERSION_DIR" "$PREVIOUS_VERSION_DIR" \
        || fail "refusing to replace existing file $adopt_link"
    ln -sT "$PREFIX/current/$adopt_member" "$adopt_link" || fail "cannot create link $adopt_link"
    printf '%s: %s was an unchanged copy of the installed file; replaced it with a link so updates reach it\n' \
        "$PROGRAM" "$adopt_link" >&2
}

ensure_link() {
    link_member=$1
    link_path=$2
    link_kind=$3
    link_target=$PREFIX/current/$link_member
    if [ -L "$link_path" ]; then
        check_link_destination "$link_path" "$link_member"
        if [ "$link_kind" = desktop ]; then
            # Refresh directory watchers after switching the version target.
            ln -snf "$link_target" "$link_path" || fail "cannot refresh link $link_path"
        fi
        return
    fi
    if [ -e "$link_path" ]; then
        [ -f "$link_path" ] || fail "refusing to replace existing file $link_path"
        adopt_managed_copy "$link_member" "$link_path" "$link_kind"
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
    ln -sT "$link_target" "$link_path" || fail "cannot create link $link_path"
}


# A reviewed worker supplies its pre-shutdown census. A standalone installer
# captures the authenticated source before it can prepare or stop that source.
if [ -n "${CAGENTS_INSTALL_CENSUS_PATH:-}" ]; then
    set -- daemon verify-update-census --census "$INSTALL_CENSUS_PATH" \
        --outcome "$INSTALL_OUTCOME_PATH"
    if [ -n "${CAGENTS_DAEMON_LEASE_FD:-}" ]; then
        # The source may already be stopped, but its private transaction and
        # source binding must still validate before accepting worker evidence.
        set -- "$@" --validate-only
    else
        # An environment path alone cannot authorize preparing a live source.
        set -- "$@" --require-original --allow-absent
    fi
    installer_tool "$CANDIDATE_COORDINATOR" "$@" >/dev/null \
        || fail 'the supplied terminal census is invalid or no longer matches the source; the service was not changed'
    CENSUS_CAPTURED=1
else
    installer_tool "$CANDIDATE_COORDINATOR" daemon capture-update-census \
        --output "$INSTALL_CENSUS_PATH" --candidate-cli "$NEW_VERSION_DIR/bin/cagents" >/dev/null \
        || fail 'cannot capture the terminal inventory; the running service was not changed'
    CENSUS_CAPTURED=1
fi
# A fresh/absent-source file install may defer its first daemon start to login.
# This discriminator is a private, authenticated absence proof, not a status
# string or merely an empty inventory. Pending recovery cannot qualify.
if [ "$STAGE_ONLY" -eq 0 ]; then
    if verify_absent_census; then
        PREUPDATE_NO_SOURCE=1
    fi
fi
OWNER_CLI=${CAGENTS_INSTALL_OWNER_CLI:-$PREFIX/current/bin/cagents}
if [ "$HAD_CURRENT" -eq 1 ]; then
    if [ "$RESTART_DAEMON" -eq 1 ]; then
        if [ -n "${CAGENTS_DAEMON_LEASE_FD:-}" ]; then
            # The reviewed worker already checkpointed the source and holds
            # its lease. The candidate validates the exact inherited lock;
            # an environment marker alone cannot authorize skipping prepare.
            installer_lease_tool "$NEW_VERSION_DIR/bin/cagents" daemon verify-update-lease >/dev/null \
                || fail 'cannot verify the terminal service is ready for the update; installation was not changed'
        else
            read_daemon_status "$OWNER_CLI"
            case $DAEMON_STATUS in
                running)
                    OLD_DAEMON_WAS_RUNNING=1
                    if [ "$REQUIRE_EMPTY_DAEMON" -eq 1 ]; then
                        stop_daemon_and_wait "$OWNER_CLI"
                        DAEMON_STOPPED_FOR_SWITCH=1
                    else
                        prepare_daemon_upgrade "$OWNER_CLI"
                    fi
                    ;;
                stopped) ;;
                *) fail 'cannot verify the installed daemon status; installation was not changed' ;;
            esac
        fi
    else
        # The candidate handshake proves protocol compatibility without changing
        # the running process. Compatible updates never replace live PTYs.
        read_daemon_status "$NEW_VERSION_DIR/bin/cagents"
        if [ "$DAEMON_STATUS" = running ]; then
            case $daemon_output in
                *'"daemon_version":""'*) DAEMON_STATUS=unknown ;;
                *'"daemon_version":"'*) ;;
                *) DAEMON_STATUS=unknown ;;
            esac
        fi
        case $DAEMON_STATUS in
            running)
                OLD_DAEMON_WAS_RUNNING=1
                ;;
            stopped) ;;
            *)
                read_daemon_status "$OWNER_CLI"
                case $DAEMON_STATUS in
                    running)
                        OLD_DAEMON_WAS_RUNNING=1
                        # Routing is recorded before current changes. The
                        # outer owner reopens the previous UI even if a later
                        # publication failure rolls this transaction back.
                        installer_tool "$COORDINATOR" record-deferred-install >/dev/null \
                            || fail 'cannot preserve the running application for a deferred update; installation was not changed'
                        DAEMON_DEFERRED=1
                        ;;
                    stopped) ;;
                    *) fail 'cannot verify the installed daemon status; installation was not changed' ;;
                esac
                ;;
        esac
    fi
fi
# The held launch fence serializes installers. Refuse an unexpected independent
# current change instead of overwriting the version captured by this transaction.
if [ "$HAD_CURRENT" -eq 1 ]; then
    if [ ! -L "$PREFIX/current" ] || [ "$(readlink "$PREFIX/current" 2>/dev/null || :)" != "$PREVIOUS_CURRENT_TARGET" ]; then
        ROLLBACK_OK=0
        fail 'current installation changed while preparing the update; retained both version trees for recovery'
    fi
else
    if [ -e "$PREFIX/current" ] || [ -L "$PREFIX/current" ]; then
        ROLLBACK_OK=0
        fail 'current installation appeared while preparing the update; retained the candidate for recovery'
    fi
fi
# The previous daemon has stopped, so the store holds the schema it reads.
# Record it before the replacement can convert the store, so that a rollback
# can convert it back. Only a replacement whose command line offers the store
# commands is asked. Its daemon is never run directly: releases before the
# options ignore arguments and would start serving. A replacement without
# them predates every conversion, so it has nothing to undo.
if [ "$HAD_CURRENT" -eq 1 ] && [ "$RESTART_DAEMON" -eq 1 ] && [ "$DAEMON_DEFERRED" -eq 0 ] \
    && supports_store_conversion "$NEW_VERSION_DIR/bin/cagents"; then
    STORE_SCHEMA_BEFORE=$(installer_lease_tool "$NEW_VERSION_DIR/bin/cagents" daemon store-schema) \
        || fail 'cannot read the shared-model store schema; installation was not changed'
    case $STORE_SCHEMA_BEFORE in
        '') STORE_SCHEMA_BEFORE=none ;;
        *[!0-9]*) fail 'the shared-model store schema is unreadable; installation was not changed' ;;
    esac
fi
CURRENT_TEMP=$PREFIX/.current.$$
rm -f "$CURRENT_TEMP"
ln -s "versions/$VERSION" "$CURRENT_TEMP"
CURRENT_SWITCHED=1
mv -Tf "$CURRENT_TEMP" "$PREFIX/current"

# Publish launchers only after their executable and icon targets exist. Menus
# can cache a dangling desktop link as missing until their next full rescan.
ensure_link "$BIN_MEMBER" "$BIN_LINK" bin
ensure_link "$DESKTOP_MEMBER" "$DESKTOP_LINK" desktop
ensure_link "$ICON_MEMBER" "$ICON_LINK" icon
ensure_link "$RASTER_ICON_MEMBER" "$RASTER_ICON_LINK" raster_icon
ensure_link "$METAINFO_MEMBER" "$METAINFO_LINK" metainfo
ensure_link "$AUTOSTART_MEMBER" "$AUTOSTART_LINK" autostart
# Startup exit status alone says nothing about retained terminals. Commit only
# after the candidate proves serving readiness and the captured census matches.
# The sole file-only exception is a proven absent source on an ordinary install.
if [ "$DAEMON_DEFERRED" -eq 1 ]; then
    : # The installed candidate must not try to start beside the older daemon.
elif DAEMON_START_ATTEMPTED=1 && installer_start_candidate >/dev/null; then
    :
else
    if [ "$PREUPDATE_NO_SOURCE" -eq 1 ] && [ "$STAGE_ONLY" -eq 0 ]; then
        if ! verify_absent_census; then
            # Startup can create a checkpoint or another service can appear.
            # Neither may be hidden by the earlier absence or pointer rollback.
            verify_update_census >/dev/null 2>&1 || :
            TRANSACTION_COMMITTED=1
            RECOVERY_OUTCOME_SETTLED=1
            retain_recovery_evidence
            fail 'daemon startup failed and absence is no longer proven; retained the selected version and recovery state'
        fi
        DAEMON_START_FAILED=1
        # Publish only file-install success, with authenticated NoSource rather
        # than Ready or recovered sessions.
        printf '%s: the daemon start command failed; graphical-login autostart remains enabled\n' \
            "$PROGRAM" >&2
    else
        fail 'replacement daemon startup failed; checking rollback safety while preserving durable recovery state'
    fi
fi
if [ "$DAEMON_START_FAILED" -eq 0 ]; then
    if ! verify_update_census; then
        retain_recovery_evidence
        fail 'the installation did not prove that the terminal service and its sessions are ready'
    fi
    if [ "$DAEMON_DEFERRED" -eq 0 ]; then DAEMON_STARTED=1; fi
fi
TRANSACTION_COMMITTED=1
if [ "$NEW_VERSION_RETAINED" -eq 1 ]; then
    rm -f "$NEW_VERSION_DIR/$RETAINED_MARKER"
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache --force --ignore-theme-index "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || :
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || :
fi


if [ "$DAEMON_DEFERRED" -eq 1 ]; then
    printf 'Installed cagents %s; running sessions were preserved. Run cagents daemon upgrade to finish updating the daemon.\n' "$VERSION"
elif [ "$DAEMON_STARTED" -eq 1 ]; then
    printf 'Installed cagents %s; the production daemon and your sessions are ready. The daemon will start at login.\n' "$VERSION"
else
    printf 'Installed cagents %s; the production daemon will start at graphical login.\n' "$VERSION"
fi
printf 'Ensure %s is on PATH, then run: cagents ui\n' "$HOME/.local/bin"
