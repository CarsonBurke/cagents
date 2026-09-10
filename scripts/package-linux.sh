#!/bin/sh
set -eu

PROGRAM=package-linux.sh
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(dirname -- "$SCRIPT_DIR")
DIST_DIR=${CAGENTS_DIST_DIR:-"$ROOT_DIR/dist"}
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
VARIANT=full

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

case $# in
    0) ;;
    2)
        [ "$1" = --variant ] || fail 'usage: package-linux.sh [--variant full|browserless]'
        VARIANT=$2
        ;;
    *) fail 'usage: package-linux.sh [--variant full|browserless]' ;;
esac
[ "$(uname -s)" = Linux ] || fail 'packaging is supported only on Linux'
[ "$(uname -m)" = x86_64 ] || fail 'packaging is supported only on x86_64'
case $(getconf GNU_LIBC_VERSION 2>/dev/null || :) in
    glibc\ *) ;;
    *) fail 'packaging requires a glibc host' ;;
esac

# Release names describe an explicitly supported native target, not whichever
# platform happened to invoke the packager. Add ports only with a real build lane.
RELEASE_TARGET=${CAGENTS_RELEASE_TARGET:-x86_64-unknown-linux-gnu}
case $RELEASE_TARGET in
    x86_64-unknown-linux-gnu) DEB_ARCH=amd64 ;;
    *) fail "unsupported release target: $RELEASE_TARGET" ;;
esac
case ${CARGO_BUILD_TARGET:-} in
    '') ;;
    *) fail 'unset CARGO_BUILD_TARGET; this script packages native release binaries' ;;
esac

if [ -z "${VERSION:-}" ]; then
    VERSION=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' "$ROOT_DIR/Cargo.toml" | sed -n '1p')
fi
case $VERSION in
    ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "unsafe package version: $VERSION" ;;
esac

case $VARIANT in
    full)
        TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target"}
        TOP_LEVEL=cagents-$VERSION
        ARCHIVE_NAME=cagents-$VERSION-$RELEASE_TARGET.tar.gz
        DEB_NAME=cagents_${VERSION}_${DEB_ARCH}.deb
        PACKAGE_NAME=cagents
        PACKAGE_SUMMARY='coding-agent workspace for Wayland'
        PACKAGE_DESCRIPTION=' cagents organizes coding-agent sessions, repositories, worktrees,
 terminals, and integrated browser surfaces in one desktop workspace.'
        COMMAND_NAME=cagents
        DESKTOP_ID=dev.cagents.Cagents
        CHECKSUM_NAME=SHA256SUMS
        SIZE_NAME=cagents-$VERSION-sizes.txt
        EXTRA_DEPENDS=
        OTHER_PACKAGE=cagents-browserless
        ;;
    browserless)
        TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target/browserless"}
        TOP_LEVEL=cagents-browserless-$VERSION
        ARCHIVE_NAME=cagents-browserless-$VERSION-$RELEASE_TARGET.tar.gz
        DEB_NAME=cagents-browserless_${VERSION}_${DEB_ARCH}.deb
        PACKAGE_NAME=cagents-browserless
        PACKAGE_SUMMARY='browserless coding-agent workspace for Wayland'
        PACKAGE_DESCRIPTION=' cagents without the integrated Servo browser; saved browser cards
 remain available for opening in the system browser.'
        COMMAND_NAME=cagents-browserless
        DESKTOP_ID=dev.cagents.Cagents.Browserless
        CHECKSUM_NAME=SHA256SUMS-browserless
        SIZE_NAME=cagents-browserless-$VERSION-sizes.txt
        EXTRA_DEPENDS=', xdg-utils'
        OTHER_PACKAGE=cagents
        ;;
    *) fail "unknown package variant: $VARIANT" ;;
esac

for command in cargo date dpkg-deb gzip install mktemp sed sha256sum sort strip tar touch wc; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

BUILD_SECONDS=skipped
if [ "${CAGENTS_SKIP_BUILD:-0}" != 1 ]; then
    BUILD_STARTED=$(date +%s)
    command -v zig >/dev/null 2>&1 || fail 'required command not found: zig'
    # Native build scripts run outside the repository and cannot rely on a mise
    # shim rediscovering mise.toml. Resolve the real executable up front.
    ZIG_ENV=$(zig env) || fail 'could not inspect the Zig installation'
    ZIG_EXECUTABLE=$(printf '%s\n' "$ZIG_ENV" | sed -n \
        -e 's/^[[:space:]]*"zig_exe":[[:space:]]*"\([^"]*\)",\{0,1\}[[:space:]]*$/\1/p' \
        -e 's/^[[:space:]]*\.zig_exe[[:space:]]*=[[:space:]]*"\([^"]*\)",\{0,1\}[[:space:]]*$/\1/p')
    case $ZIG_EXECUTABLE in
        /*) ;;
        *) fail 'zig env did not report an absolute zig_exe' ;;
    esac
    [ -x "$ZIG_EXECUTABLE" ] || fail "zig executable is not runnable: $ZIG_EXECUTABLE"
    PATH=${ZIG_EXECUTABLE%/*}:$PATH
    export PATH

    build_target() {
        cargo build \
            --manifest-path "$ROOT_DIR/Cargo.toml" \
            --locked \
            --release \
            --target-dir "$TARGET_DIR" \
            "$@"
    }

    # Keep package feature sets disjoint. A single invocation would unify
    # package features and compile/link both host and UI implementation graphs.
    if [ "$VARIANT" = browserless ]; then
        build_target --no-default-features --bin cagents
        build_target --no-default-features --features host --bin cagentsd
        build_target --no-default-features --features ui --bin cagents-ui
    else
        build_target --bin cagents
        build_target --features host --bin cagentsd
        build_target --features integrated-browser --bin cagents-ui
    fi
    BUILD_FINISHED=$(date +%s)
    BUILD_SECONDS=$((BUILD_FINISHED - BUILD_STARTED))
fi

# Report the package's whole resolved graph: the union of the lanes built above.
# `cargo tree` only reads Cargo.lock, so unifying the features here resolves
# nothing that a build would compile. Never copy these feature sets into a
# build command.
if [ "$VARIANT" = browserless ]; then
    RESOLVED_PACKAGE_IDS=$(
        cargo tree --manifest-path "$ROOT_DIR/Cargo.toml" --locked \
            --no-default-features --features ui,host --edges normal --prefix none |
            sed 's/ (\*)$//' |
            LC_ALL=C sort -u |
            wc -l
    )
else
    RESOLVED_PACKAGE_IDS=$(
        cargo tree --manifest-path "$ROOT_DIR/Cargo.toml" --locked \
            --features host,integrated-browser --edges normal --prefix none |
            sed 's/ (\*)$//' |
            LC_ALL=C sort -u |
            wc -l
    )
fi
RESOLVED_PACKAGE_IDS=$((RESOLVED_PACKAGE_IDS))

for binary in cagents cagents-ui cagentsd; do
    [ -x "$TARGET_DIR/release/$binary" ] || fail "missing release binary: $TARGET_DIR/release/$binary"
done

PROTOCOL_VERSION=$("$TARGET_DIR/release/cagents" daemon protocol-version) \
    || fail 'cannot inspect packaged daemon protocol'
case $PROTOCOL_VERSION in
    ''|*[!0-9]*) fail 'packaged CLI returned an invalid daemon protocol version' ;;
esac

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/cagents-package.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

PACKAGE_ROOT=$WORK_DIR/$TOP_LEVEL
BIN_DIR=$PACKAGE_ROOT/bin
SHARE_DIR=$PACKAGE_ROOT/share

install -d -m 0755 \
    "$BIN_DIR" \
    "$SHARE_DIR/applications" \
    "$SHARE_DIR/autostart" \
    "$SHARE_DIR/icons/hicolor/scalable/apps" \
    "$SHARE_DIR/icons/hicolor/128x128/apps" \
    "$SHARE_DIR/metainfo" \
    "$SHARE_DIR/doc/cagents"
install -m 0755 "$TARGET_DIR/release/cagents" "$BIN_DIR/cagents"
install -m 0755 "$TARGET_DIR/release/cagents-ui" "$BIN_DIR/cagents-ui"
install -m 0755 "$TARGET_DIR/release/cagentsd" "$BIN_DIR/cagentsd"
install -m 0644 /dev/null "$BIN_DIR/.cagents-ui-lease-v1"
if [ "$VARIANT" = full ]; then
    install -m 0755 "$ROOT_DIR/packaging/cagents-uninstall" "$BIN_DIR/cagents-uninstall"
else
    ln -s cagents "$BIN_DIR/cagents-browserless"
fi
strip --strip-unneeded "$BIN_DIR/cagents" "$BIN_DIR/cagents-ui" "$BIN_DIR/cagentsd"
sed \
    -e "s/^Name=cagents\$/Name=$PACKAGE_NAME/" \
    -e "s/^Exec=cagents ui\$/Exec=$COMMAND_NAME ui/" \
    -e "s/^TryExec=cagents\$/TryExec=$COMMAND_NAME/" \
    -e "s/^Icon=dev\\.cagents\\.Cagents\$/Icon=$DESKTOP_ID/" \
    -e "s/^StartupWMClass=dev\\.cagents\\.Cagents\$/StartupWMClass=$DESKTOP_ID/" \
    "$ROOT_DIR/packaging/dev.cagents.Cagents.desktop" \
    > "$SHARE_DIR/applications/$DESKTOP_ID.desktop"
sed \
    -e "s/^Exec=cagents /Exec=$COMMAND_NAME /" \
    -e "s/^TryExec=cagents\$/TryExec=$COMMAND_NAME/" \
    "$ROOT_DIR/packaging/dev.cagents.Cagents.Daemon.desktop" \
    > "$SHARE_DIR/autostart/$DESKTOP_ID.Daemon.desktop"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.svg" "$SHARE_DIR/icons/hicolor/scalable/apps/$DESKTOP_ID.svg"
install -m 0644 "$ROOT_DIR/packaging/dev.cagents.Cagents.png" "$SHARE_DIR/icons/hicolor/128x128/apps/$DESKTOP_ID.png"
sed \
    -e "s/dev\\.cagents\\.Cagents/$DESKTOP_ID/g" \
    -e "s|<name>cagents</name>|<name>$PACKAGE_NAME</name>|" \
    -e "s|<binary>cagents</binary>|<binary>$COMMAND_NAME</binary>|" \
    "$ROOT_DIR/packaging/dev.cagents.Cagents.metainfo.xml" \
    > "$SHARE_DIR/metainfo/$DESKTOP_ID.metainfo.xml"
install -m 0644 "$ROOT_DIR/LICENSE" "$SHARE_DIR/doc/cagents/LICENSE"


install -d -m 0755 "$DIST_DIR"
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
    "$DEB_ROOT/etc/xdg/autostart" \
    "$DEB_ROOT/usr/bin" \
    "$DEB_ROOT/usr/libexec/$PACKAGE_NAME" \
    "$DEB_ROOT/usr/share/applications" \
    "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps" \
    "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps" \
    "$DEB_ROOT/usr/share/metainfo" \
    "$DEB_ROOT/usr/share/doc/$PACKAGE_NAME"
install -m 0755 "$BIN_DIR/cagents" "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/cagents"
install -m 0755 "$BIN_DIR/cagents-ui" "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/cagents-ui"
install -m 0755 "$BIN_DIR/cagentsd" "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/cagentsd"
install -m 0644 "$BIN_DIR/.cagents-ui-lease-v1" "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/.cagents-ui-lease-v1"
DAEMON_DIGEST=$(sha256sum "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/cagentsd")
DAEMON_DIGEST=${DAEMON_DIGEST%% *}
printf '%s %s\n' "$PROTOCOL_VERSION" "$DAEMON_DIGEST" > "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/.cagents-daemon-protocol"
chmod 0644 "$DEB_ROOT/usr/libexec/$PACKAGE_NAME/.cagents-daemon-protocol"
ln -s "../libexec/$PACKAGE_NAME/cagents" "$DEB_ROOT/usr/bin/$COMMAND_NAME"
sed \
    -e "s|^Exec=.*|Exec=/usr/bin/$COMMAND_NAME ui|" \
    -e "s|^TryExec=.*|TryExec=/usr/bin/$COMMAND_NAME|" \
    "$SHARE_DIR/applications/$DESKTOP_ID.desktop" \
    > "$DEB_ROOT/usr/share/applications/$DESKTOP_ID.desktop"
chmod 0644 "$DEB_ROOT/usr/share/applications/$DESKTOP_ID.desktop"
sed \
    -e "s|^Exec=.*|Exec=/usr/bin/$COMMAND_NAME daemon start|" \
    -e "s|^TryExec=.*|TryExec=/usr/bin/$COMMAND_NAME|" \
    "$SHARE_DIR/autostart/$DESKTOP_ID.Daemon.desktop" \
    > "$DEB_ROOT/etc/xdg/autostart/$DESKTOP_ID.Daemon.desktop"
chmod 0644 "$DEB_ROOT/etc/xdg/autostart/$DESKTOP_ID.Daemon.desktop"
install -m 0644 "$SHARE_DIR/icons/hicolor/scalable/apps/$DESKTOP_ID.svg" "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps/$DESKTOP_ID.svg"
install -m 0644 "$SHARE_DIR/icons/hicolor/128x128/apps/$DESKTOP_ID.png" "$DEB_ROOT/usr/share/icons/hicolor/128x128/apps/$DESKTOP_ID.png"
install -m 0644 "$SHARE_DIR/metainfo/$DESKTOP_ID.metainfo.xml" "$DEB_ROOT/usr/share/metainfo/$DESKTOP_ID.metainfo.xml"
install -m 0644 "$SHARE_DIR/doc/cagents/LICENSE" "$DEB_ROOT/usr/share/doc/$PACKAGE_NAME/copyright"

cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: $PACKAGE_NAME
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: cagents contributors <maintainers@cagents.dev>
Depends: git, libc6 (>= 2.35), libfontconfig1, libfreetype6, libgcc-s1, libstdc++6, libwayland-client0, libxkbcommon0, zlib1g$EXTRA_DEPENDS
Recommends: gh
Section: devel
Priority: optional
Description: $PACKAGE_SUMMARY
$PACKAGE_DESCRIPTION
EOF
for phase in preinst prerm; do
    case $phase in preinst) mode=upgrade ;; prerm) mode=remove ;; esac
    sed \
        -e "s|@MODE@|$mode|g" \
        -e "s|@PACKAGE_NAME@|$PACKAGE_NAME|g" \
        -e "s|@OTHER_PACKAGE@|$OTHER_PACKAGE|g" \
        -e "s|@PROTOCOL_VERSION@|$PROTOCOL_VERSION|g" \
        "$ROOT_DIR/packaging/cagents-package-lifecycle" > "$DEB_ROOT/DEBIAN/$phase"
    chmod 0755 "$DEB_ROOT/DEBIAN/$phase"
done
find "$DEB_ROOT" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH dpkg-deb --root-owner-group --build "$DEB_ROOT" "$DIST_DIR/$DEB_NAME"

INSTALLED_BYTES=0
for binary in cagents cagents-ui cagentsd; do
    binary_bytes=$(wc -c < "$BIN_DIR/$binary")
    INSTALLED_BYTES=$((INSTALLED_BYTES + binary_bytes))
done
ARCHIVE_BYTES=$(wc -c < "$DIST_DIR/$ARCHIVE_NAME")
DEB_BYTES=$(wc -c < "$DIST_DIR/$DEB_NAME")
cat > "$DIST_DIR/$SIZE_NAME" <<EOF
variant=$VARIANT
target=$RELEASE_TARGET
resolved_package_ids=$RESOLVED_PACKAGE_IDS
build_seconds=$BUILD_SECONDS
installed_binary_bytes=$INSTALLED_BYTES
portable_archive_bytes=$ARCHIVE_BYTES
debian_package_bytes=$DEB_BYTES
EOF
touch -d "@$SOURCE_DATE_EPOCH" "$DIST_DIR/$SIZE_NAME"

if [ "$VARIANT" = full ]; then
    install -m 0755 "$ROOT_DIR/install.sh" "$DIST_DIR/install.sh"
    touch -d "@$SOURCE_DATE_EPOCH" "$DIST_DIR/install.sh"
    (
        cd "$DIST_DIR"
        sha256sum "$ARCHIVE_NAME" "$DEB_NAME" "$SIZE_NAME" install.sh > "$CHECKSUM_NAME"
    )
    printf 'Created %s package:\n  %s\n  %s\n  %s\n  %s\n  %s\n' \
        "$VARIANT" \
        "$DIST_DIR/$ARCHIVE_NAME" \
        "$DIST_DIR/$DEB_NAME" \
        "$DIST_DIR/$SIZE_NAME" \
        "$DIST_DIR/$CHECKSUM_NAME" \
        "$DIST_DIR/install.sh"
else
    (
        cd "$DIST_DIR"
        sha256sum "$ARCHIVE_NAME" "$DEB_NAME" "$SIZE_NAME" > "$CHECKSUM_NAME"
    )
    printf 'Created %s package:\n  %s\n  %s\n  %s\n  %s\n' \
        "$VARIANT" \
        "$DIST_DIR/$ARCHIVE_NAME" \
        "$DIST_DIR/$DEB_NAME" \
        "$DIST_DIR/$SIZE_NAME" \
        "$DIST_DIR/$CHECKSUM_NAME"
fi
printf 'Measurements (%s): %s resolved package IDs; build %s seconds; installed binaries %s bytes; archive %s bytes; deb %s bytes\n' \
    "$VARIANT" "$RESOLVED_PACKAGE_IDS" "$BUILD_SECONDS" "$INSTALLED_BYTES" "$ARCHIVE_BYTES" "$DEB_BYTES"
