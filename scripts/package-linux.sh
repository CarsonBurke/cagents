#!/bin/sh
set -eu

PROGRAM=package-linux.sh
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(dirname -- "$SCRIPT_DIR")
DIST_DIR=$ROOT_DIR/dist
TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target"}
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

[ "$#" -eq 0 ] || fail 'this script takes no positional arguments'
[ "$(uname -s)" = Linux ] || fail 'packaging is supported only on Linux'
[ "$(uname -m)" = x86_64 ] || fail 'packaging is supported only on x86_64'
case $(getconf GNU_LIBC_VERSION 2>/dev/null || :) in
    glibc\ *) ;;
    *) fail 'packaging requires a glibc host' ;;
esac

if [ -z "${VERSION:-}" ]; then
    VERSION=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "$ROOT_DIR/Cargo.toml" | sed -n '1p')
fi
case $VERSION in
    ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "unsafe package version: $VERSION" ;;
esac

for command in cargo dpkg-deb gzip install mktemp sha256sum strip tar touch; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

if [ "${CAGENTS_SKIP_BUILD:-0}" != 1 ]; then
    cargo build \
        --manifest-path "$ROOT_DIR/Cargo.toml" \
        --locked \
        --release \
        --bin cagents \
        --bin cagents-ui \
        --bin cagentsd
fi

for binary in cagents cagents-ui cagentsd; do
    [ -x "$TARGET_DIR/release/$binary" ] || fail "missing release binary: $TARGET_DIR/release/$binary"
done

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cagents-package.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

TOP_LEVEL=cagents-$VERSION
PACKAGE_ROOT=$WORK_DIR/$TOP_LEVEL
BIN_DIR=$PACKAGE_ROOT/bin
SHARE_DIR=$PACKAGE_ROOT/share

install -d -m 0755 \
    "$BIN_DIR" \
    "$SHARE_DIR/applications" \
    "$SHARE_DIR/icons/hicolor/scalable/apps" \
    "$SHARE_DIR/icons/hicolor/128x128/apps" \
    "$SHARE_DIR/metainfo" \
    "$SHARE_DIR/doc/cagents"
install -m 0755 "$TARGET_DIR/release/cagents" "$BIN_DIR/cagents"
install -m 0755 "$TARGET_DIR/release/cagents-ui" "$BIN_DIR/cagents-ui"
install -m 0755 "$TARGET_DIR/release/cagentsd" "$BIN_DIR/cagentsd"
install -m 0755 "$ROOT_DIR/packaging/cagents-uninstall" "$BIN_DIR/cagents-uninstall"
strip --strip-unneeded "$BIN_DIR/cagents" "$BIN_DIR/cagents-ui" "$BIN_DIR/cagentsd"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.desktop" "$SHARE_DIR/applications/dev.cagents.Cagents.desktop"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.svg" "$SHARE_DIR/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.png" "$SHARE_DIR/icons/hicolor/128x128/apps/dev.cagents.Cagents.png"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.metainfo.xml" "$SHARE_DIR/metainfo/dev.cagents.Cagents.metainfo.xml"
install -m 0644 "$ROOT_DIR/LICENSE" "$SHARE_DIR/doc/cagents/LICENSE"

find "$PACKAGE_ROOT" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

rm -rf "$DIST_DIR"
install -d -m 0755 "$DIST_DIR"
ARCHIVE_NAME=cagents-$VERSION-x86_64-unknown-linux-gnu.tar.gz
PORTABLE_TAR=$WORK_DIR/portable.tar
(
    cd "$WORK_DIR"
    tar \
        --sort=name \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --format=gnu \
        -cf "$PORTABLE_TAR" \
        "$TOP_LEVEL"
)
gzip -n -9 -c "$PORTABLE_TAR" > "$DIST_DIR/$ARCHIVE_NAME"

DEB_ROOT=$WORK_DIR/deb
install -d -m 0755 \
    "$DEB_ROOT/DEBIAN" \
    "$DEB_ROOT/usr/bin" \
    "$DEB_ROOT/usr/libexec/cagents" \
    "$DEB_ROOT/usr/share/applications" \
    "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps" \
    "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps" \
    "$DEB_ROOT/usr/share/metainfo" \
    "$DEB_ROOT/usr/share/doc/cagents"
install -m 0755 "$BIN_DIR/cagents" "$DEB_ROOT/usr/libexec/cagents/cagents"
install -m 0755 "$BIN_DIR/cagents-ui" "$DEB_ROOT/usr/libexec/cagents/cagents-ui"
install -m 0755 "$BIN_DIR/cagentsd" "$DEB_ROOT/usr/libexec/cagents/cagentsd"
ln -s ../libexec/cagents/cagents "$DEB_ROOT/usr/bin/cagents"
install -m 0644 "$SHARE_DIR/applications/dev.cagents.Cagents.desktop" "$DEB_ROOT/usr/share/applications/dev.cagents.Cagents.desktop"
install -m 0644 "$SHARE_DIR/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg" "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"
install -m 0644 "$SHARE_DIR/icons/hicolor/128x128/apps/dev.cagents.Cagents.png" "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png"
install -m 0644 "$SHARE_DIR/metainfo/dev.cagents.Cagents.metainfo.xml" "$DEB_ROOT/usr/share/metainfo/dev.cagents.Cagents.metainfo.xml"
install -m 0644 "$SHARE_DIR/doc/cagents/LICENSE" "$DEB_ROOT/usr/share/doc/cagents/copyright"

cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: cagents
Version: $VERSION
Architecture: amd64
Maintainer: cagents contributors <maintainers@cagents.dev>
Depends: git, libc6 (>= 2.35), libfontconfig1, libfreetype6, libgcc-s1, libstdc++6, libwayland-client0, libxkbcommon0, zlib1g
Recommends: gh
Section: devel
Priority: optional
Description: coding-agent workspace for Wayland
 cagents organizes coding-agent sessions, repositories, worktrees,
 terminals, and browser surfaces in one desktop workspace.
EOF
cat > "$DEB_ROOT/DEBIAN/preinst" <<'EOF'
#!/bin/sh
set -e

is_cagentsd_pid() {
    pid=$1
    case $pid in
        ''|*[!0-9]*) return 1 ;;
    esac
    executable=$(readlink "/proc/$pid/exe" 2>/dev/null) || return 1
    case $executable in
        /usr/libexec/cagents/cagentsd|'/usr/libexec/cagents/cagentsd (deleted)')
            return 0
            ;;
        *) return 1 ;;
    esac
}

stop_cagentsd() {
    cagentsd_pids=
    for process in /proc/[0-9]*; do
        pid=${process##*/}
        if is_cagentsd_pid "$pid"; then
            cagentsd_pids="$cagentsd_pids $pid"
        fi
    done

    for pid in $cagentsd_pids; do
        if is_cagentsd_pid "$pid"; then
            kill -TERM "$pid" 2>/dev/null || :
        fi
    done

    waited=0
    while :; do
        remaining_pids=
        for pid in $cagentsd_pids; do
            if is_cagentsd_pid "$pid"; then
                remaining_pids="$remaining_pids $pid"
            fi
        done
        [ -n "$remaining_pids" ] || return 0
        if [ "$waited" -ge 30 ]; then
            printf '%s\n' 'cagents: timed out waiting for cagentsd to stop' >&2
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
        cagentsd_pids=$remaining_pids
    done
}

stop_cagentsd
exit 0
EOF
cat > "$DEB_ROOT/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e

is_cagentsd_pid() {
    pid=$1
    case $pid in
        ''|*[!0-9]*) return 1 ;;
    esac
    executable=$(readlink "/proc/$pid/exe" 2>/dev/null) || return 1
    case $executable in
        /usr/libexec/cagents/cagentsd|'/usr/libexec/cagents/cagentsd (deleted)')
            return 0
            ;;
        *) return 1 ;;
    esac
}

stop_cagentsd() {
    cagentsd_pids=
    for process in /proc/[0-9]*; do
        pid=${process##*/}
        if is_cagentsd_pid "$pid"; then
            cagentsd_pids="$cagentsd_pids $pid"
        fi
    done

    for pid in $cagentsd_pids; do
        if is_cagentsd_pid "$pid"; then
            kill -TERM "$pid" 2>/dev/null || :
        fi
    done

    waited=0
    while :; do
        remaining_pids=
        for pid in $cagentsd_pids; do
            if is_cagentsd_pid "$pid"; then
                remaining_pids="$remaining_pids $pid"
            fi
        done
        [ -n "$remaining_pids" ] || return 0
        if [ "$waited" -ge 30 ]; then
            printf '%s\n' 'cagents: timed out waiting for cagentsd to stop' >&2
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
        cagentsd_pids=$remaining_pids
    done
}

stop_cagentsd
exit 0
EOF
chmod 0755 "$DEB_ROOT/DEBIAN/preinst" "$DEB_ROOT/DEBIAN/prerm"
find "$DEB_ROOT" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

DEB_NAME=cagents_${VERSION}_amd64.deb
SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH dpkg-deb --root-owner-group --build "$DEB_ROOT" "$DIST_DIR/$DEB_NAME"
install -m 0755 "$ROOT_DIR/install.sh" "$DIST_DIR/install.sh"
touch -d "@$SOURCE_DATE_EPOCH" "$DIST_DIR/install.sh"
(
    cd "$DIST_DIR"
    sha256sum "$ARCHIVE_NAME" "$DEB_NAME" install.sh > SHA256SUMS
)

printf 'Created:\n  %s\n  %s\n  %s\n  %s\n' \
    "$DIST_DIR/$ARCHIVE_NAME" \
    "$DIST_DIR/$DEB_NAME" \
    "$DIST_DIR/SHA256SUMS" \
    "$DIST_DIR/install.sh"
