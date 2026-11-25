#!/bin/sh
# Install, upgrade or remove cagents for the current user on Apple Silicon
# macOS. See docs/releasing.md#macos for the layout.
set -eu

PROGRAM=cagents-install
DEFAULT_REPOSITORY='<OWNER>/<REPO>'
TARGET=aarch64-apple-darwin
LABEL=dev.cagents.Cagents.Daemon
MARKER_TEXT='cagents install root'

usage() {
    cat <<'EOF'
Usage: install-macos.sh [OPTIONS]

Install the cagents Apple Silicon macOS release for the current user.

  ~/Library/Application Support/cagents-install   versions, and Cagents.app
  ~/Applications/Cagents.app                      a link to Cagents.app
  ~/.local/bin/cagents                            the command line
  ~/Library/LaunchAgents/dev.cagents.Cagents.Daemon.plist
                                                  starts the daemon at login

A daemon of another version must be stopped first; the installer says how.

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
while [ "$#" -gt 0 ]; do
    case $1 in
        --version|--prefix)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"
            case $1 in
                --version) VERSION=${2#v} ;;
                --prefix) PREFIX=$2 ;;
            esac
            shift 2
            ;;
        --no-start) START=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) UNINSTALL=1; PURGE=1; shift ;;
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
for command in awk cmp codesign curl diff ditto find launchctl ln mkdir mktemp mv osascript plutil rm shasum tar; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

PREFIX=${PREFIX:-"$HOME/Library/Application Support/cagents-install"}
case $PREFIX in
    /*) ;;
    *) fail '--prefix must be an absolute path' ;;
esac
PREFIX=${PREFIX%/}
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
register_app() {
    [ "$HOME" = "$ACCOUNT_HOME" ] && [ -x "$LSREGISTER" ] || return 0
    "$LSREGISTER" "$@" >/dev/null 2>&1 || :
}

# The installation this script made, or nothing at PREFIX at all.
owned_prefix() {
    [ -f "$PREFIX/.cagents-install-root" ] && [ ! -L "$PREFIX/.cagents-install-root" ] \
        && [ "$(cat "$PREFIX/.cagents-install-root")" = "$MARKER_TEXT" ]
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

# Whether the login item at AGENT_PLIST runs a version of this installation.
owned_agent() {
    [ -f "$AGENT_PLIST" ] || return 1
    program=$("$PLIST_BUDDY" -c 'Print :ProgramArguments:0' "$AGENT_PLIST" 2>/dev/null) || return 1
    case $program in
        "$PREFIX/versions/"*) return 0 ;;
        *) return 1 ;;
    esac
}

# A second installer working on PREFIX at once would interleave its steps.
LOCK=
acquire_lock() {
    LOCK=$PREFIX/.lock
    if ! mkdir "$LOCK" 2>/dev/null; then
        holder=$(cat "$LOCK/pid" 2>/dev/null || :)
        if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
            LOCK=
            fail "another cagents installer (process $holder) is changing $PREFIX; wait for it to finish"
        fi
        rm -rf "$LOCK"
        mkdir "$LOCK" 2>/dev/null || { LOCK=; fail "cannot lock $PREFIX"; }
    fi
    printf '%s\n' "$$" > "$LOCK/pid"
}
release_lock() {
    [ -z "$LOCK" ] || rm -rf "$LOCK"
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
    if [ -e "$PREFIX" ]; then
        owned_prefix || fail "$PREFIX was not made by this installer; nothing was removed"
        acquire_lock
        trap release_lock EXIT
        trap 'exit 1' HUP INT TERM
        if [ -x "$PREFIX/current/bin/cagents" ]; then
            daemon=$(running_daemon "$PREFIX/current/bin/cagents") \
                || fail 'cannot tell whether the cagents daemon is running; nothing was removed'
            if [ -n "$daemon" ]; then
                printf '%s: the cagents daemon (%s) is running; nothing was removed.\n' "$PROGRAM" "$daemon" >&2
                stop_instructions >&2
                exit 1
            fi
        elif [ "$PURGE" -eq 1 ]; then
            fail "$PREFIX has no installed cagents to check for a running daemon; nothing was removed"
        fi
    elif [ "$PURGE" -eq 1 ]; then
        fail "no cagents is installed at $PREFIX to check for a running daemon; stop it with \`cagents daemon stop\`, then remove ~/Library/Application Support/dev.cagents.cagents yourself"
    fi
    if owned_agent; then
        unload_agent
        rm -f "$AGENT_PLIST"
    fi
    remove_owned_link "$BIN_LINK" "$PREFIX/current/bin/cagents"
    remove_owned_link "$APP_LINK" "$APP"
    if [ -e "$PREFIX" ]; then
        [ ! -d "$APP" ] || register_app -u "$APP"
        rm -rf "$PREFIX/versions" "$PREFIX/current" "$APP" "$PREFIX/.update" "$PREFIX"/.stage.* "$PREFIX"/.current.* \
            "$PREFIX/.previous"
        rm -f "$PREFIX/.cagents-install-root"
        release_lock
        LOCK=
        rmdir "$PREFIX" 2>/dev/null || note "kept $PREFIX, which holds files this installer did not make"
    fi
    if [ "$PURGE" -eq 1 ]; then
        # ProjectDirs("dev", "cagents", "cagents") on macOS.
        rm -rf "$HOME/Library/Application Support/dev.cagents.cagents" \
            "$HOME/Library/Caches/dev.cagents.cagents" \
            "$HOME/Library/Preferences/dev.cagents.cagents"
        runtime=$(getconf DARWIN_USER_DIR 2>/dev/null || :)
        [ -z "$runtime" ] || rm -rf "${runtime%/}/cagents/cagents"
        printf 'Removed cagents and its data.\n'
    else
        printf 'Removed cagents; your data in ~/Library/Application Support/dev.cagents.cagents was kept.\n'
    fi
    exit 0
fi

REPOSITORY=${CAGENTS_REPOSITORY:-$DEFAULT_REPOSITORY}
case $REPOSITORY in
    '<OWNER>/<REPO>'|'') fail 'set CAGENTS_REPOSITORY to the GitHub OWNER/REPO for this project' ;;
    */*/*|*[!A-Za-z0-9._/-]*) fail "invalid repository: $REPOSITORY" ;;
esac
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

if [ -e "$PREFIX" ] || [ -L "$PREFIX" ]; then
    [ -d "$PREFIX" ] && [ ! -L "$PREFIX" ] || fail "$PREFIX is not a directory"
    if ! owned_prefix; then
        [ -z "$(ls -A "$PREFIX")" ] || fail "$PREFIX holds files this installer did not make; choose another --prefix"
    fi
fi
mkdir -p "$PREFIX/versions"
chmod 0755 "$PREFIX"
[ "$(stat -f %u "$PREFIX")" = "$(id -u)" ] || fail "$PREFIX is not owned by you"
owned_prefix || printf '%s\n' "$MARKER_TEXT" > "$PREFIX/.cagents-install-root"
acquire_lock

STAGE=$(mktemp -d "$PREFIX/.stage.XXXXXX")
cleanup() {
    rm -rf "$STAGE"
    release_lock
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

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
LC_ALL=C sort -o "$STAGE/members" "$STAGE/members"
for member in \
    '' bin/ bin/cagents bin/cagents-dictate bin/cagents-ui bin/cagentsd \
    Cagents.app/ Cagents.app/Contents/ Cagents.app/Contents/Info.plist \
    Cagents.app/Contents/MacOS/ Cagents.app/Contents/MacOS/Cagents Cagents.app/Contents/PkgInfo \
    Cagents.app/Contents/Resources/ Cagents.app/Contents/Resources/Cagents.icns \
    Cagents.app/Contents/_CodeSignature/ Cagents.app/Contents/_CodeSignature/CodeResources \
    share/ share/doc/ share/doc/cagents/ share/doc/cagents/LICENSE share/doc/cagents/THIRD-PARTY-NOTICES
do
    printf '%s\n' "$TOP_LEVEL/$member"
done | sed 's|/$||' | LC_ALL=C sort > "$STAGE/expected"
sed 's|/$||' "$STAGE/members" | LC_ALL=C sort | cmp -s - "$STAGE/expected" \
    || fail "$ARCHIVE_NAME has missing or unexpected members"
mkdir "$STAGE/tree"
tar -xzf "$STAGE/$ARCHIVE_NAME" -C "$STAGE/tree" --no-same-owner || fail "cannot unpack $ARCHIVE_NAME"
NEW=$STAGE/tree/$TOP_LEVEL
find "$NEW" -type l | grep -q . && fail "$ARCHIVE_NAME holds symbolic links"
# A download through a browser carries quarantine, which Gatekeeper would
# turn into a refusal for an ad hoc signed app the person chose to install.
xattr -dr com.apple.quarantine "$NEW" 2>/dev/null || :
for path in "$NEW/bin/cagents" "$NEW/bin/cagentsd" "$NEW/bin/cagents-ui" "$NEW/bin/cagents-dictate" "$NEW/Cagents.app"; do
    codesign --verify --strict "$path" 2>/dev/null || fail "${path#"$NEW/"} is not intact; download the release again"
done
reported=$("$NEW/bin/cagents" --version) || fail 'the downloaded cagents does not run on this Mac'
[ "$reported" = "cagents $VERSION" ] || fail "the archive holds $reported, not cagents $VERSION"

# Only one daemon serves a user, and macOS cannot hand its sessions to
# another version yet: install over a running daemon only of this version.
CURRENT_VERSION=
if [ -L "$PREFIX/current" ]; then
    CURRENT_VERSION=$(readlink "$PREFIX/current")
    CURRENT_VERSION=${CURRENT_VERSION#versions/}
fi
daemon=$(installed_daemon "$NEW/bin/cagents") \
    || fail 'cannot tell whether the cagents daemon is running; the installation was not changed'
if [ -n "$daemon" ] && [ "$daemon" != "$VERSION" ]; then
    printf '%s: the cagents daemon (%s) is running; the installation was not changed.\n' "$PROGRAM" "$daemon" >&2
    stop_instructions >&2
    exit 1
fi

TARGET_DIR=$PREFIX/versions/$VERSION
if [ -e "$TARGET_DIR" ] || [ -L "$TARGET_DIR" ]; then
    # This release again, as a repair or a return to the version kept for
    # rollback: keep the tree when it is the release's, or replace it whole.
    # A process running from the old tree keeps the files it has open.
    if [ -L "$TARGET_DIR" ] || ! diff -rq "$NEW" "$TARGET_DIR" >/dev/null 2>&1; then
        mv "$TARGET_DIR" "$STAGE/replaced"
        mv "$NEW" "$TARGET_DIR"
    fi
else
    mv "$NEW" "$TARGET_DIR"
fi
# The version kept for rollback: the one this install replaces, or after a
# reinstall the one recorded before.
PREVIOUS_VERSION=$(cat "$PREFIX/.previous" 2>/dev/null || :)
if [ -n "$CURRENT_VERSION" ] && [ "$CURRENT_VERSION" != "$VERSION" ]; then
    PREVIOUS_VERSION=$CURRENT_VERSION
fi
[ "$PREVIOUS_VERSION" != "$VERSION" ] || PREVIOUS_VERSION=
rm -f "$PREFIX/.current.$$"
ln -s "versions/$VERSION" "$PREFIX/.current.$$"
mv -fh "$PREFIX/.current.$$" "$PREFIX/current"
printf '%s\n' "$PREVIOUS_VERSION" > "$PREFIX/.previous"

# Cagents.app stays at one path, which the Dock and Login Items remember. Replace it only when the release changed it.
if [ ! -d "$APP" ] || ! diff -rq "$TARGET_DIR/Cagents.app" "$APP" >/dev/null 2>&1; then
    rm -rf "$PREFIX/.Cagents.app.new" "$PREFIX/.Cagents.app.old"
    ditto "$TARGET_DIR/Cagents.app" "$PREFIX/.Cagents.app.new"
    [ ! -e "$APP" ] || mv "$APP" "$PREFIX/.Cagents.app.old"
    mv "$PREFIX/.Cagents.app.new" "$APP"
    rm -rf "$PREFIX/.Cagents.app.old"
    register_app -f "$APP"
fi

mkdir -p "$HOME/Applications" "$HOME/.local/bin"
link() {
    if [ -L "$2" ]; then
        rm -f "$2"
    elif [ -e "$2" ]; then
        note "kept the existing $2; it is not a link this installer made"
        return 0
    fi
    ln -s "$1" "$2"
}
link "$APP" "$APP_LINK"
link "$PREFIX/current/bin/cagents" "$BIN_LINK"

# The login item runs `cagents daemon start` once at login, as the XDG
# autostart entry does on Linux. It names the version's real path: on macOS
# an executable sees the path it was started by, and a daemon started
# through `current` would look for its companions in the next version.
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$STAGE/agent.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$LABEL</string>
	<key>ProgramArguments</key>
	<array>
		<string>$(printf '%s' "$TARGET_DIR/bin/cagents" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g')</string>
		<string>daemon</string>
		<string>start</string>
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
plutil -lint -s "$STAGE/agent.plist" || fail 'could not write the login item'
mv -f "$STAGE/agent.plist" "$AGENT_PLIST"
chmod 0644 "$AGENT_PLIST"
DAEMON_STARTED=0
if [ "$START" -eq 1 ]; then
    # A job loaded from an earlier install would keep its old definition.
    unload_agent
    if launchctl bootstrap "$DOMAIN" "$AGENT_PLIST" 2>/dev/null; then
        # RunAtLoad starts the daemon now; wait briefly to report it.
        attempt=0
        while [ "$attempt" -lt 20 ]; do
            if [ -n "$(running_daemon "$TARGET_DIR/bin/cagents" 2>/dev/null)" ]; then
                DAEMON_STARTED=1
                break
            fi
            attempt=$((attempt + 1))
            sleep 1
        done
    else
        note 'could not load the login item now; the daemon will start at your next login'
    fi
fi

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
