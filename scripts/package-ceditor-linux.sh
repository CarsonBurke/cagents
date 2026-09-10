#!/bin/sh
set -eu

PROGRAM=package-ceditor-linux.sh
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT_DIR=$(dirname -- "$SCRIPT_DIR")
DIST_DIR=${CAGENTS_DIST_DIR:-"$ROOT_DIR/dist"}
TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target"}
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
RELEASE_TARGET=${CAGENTS_RELEASE_TARGET:-x86_64-unknown-linux-gnu}
DESKTOP_ID=io.github.cagents.ceditor

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

case $# in
    0) ;;
    1)
        [ "$1" = --help ] || fail 'usage: package-ceditor-linux.sh [--help]'
        printf '%s\n' 'Build a standalone ceditor Linux archive with its desktop entry and icon.' \
            'Environment: VERSION, CAGENTS_RELEASE_TARGET, CAGENTS_DIST_DIR, CARGO_TARGET_DIR,' \
            'SOURCE_DATE_EPOCH, CAGENTS_SKIP_BUILD=1 (package an existing release binary).'
        exit 0
        ;;
    *) fail 'usage: package-ceditor-linux.sh [--help]' ;;
esac

[ "$(uname -s)" = Linux ] || fail 'packaging is supported only on Linux'
[ "$(uname -m)" = x86_64 ] || fail 'packaging is supported only on x86_64'
case $(getconf GNU_LIBC_VERSION 2>/dev/null || :) in
    glibc\ *) ;;
    *) fail 'packaging requires a glibc host' ;;
esac
case $RELEASE_TARGET in
    x86_64-unknown-linux-gnu) ;;
    *) fail "unsupported release target: $RELEASE_TARGET" ;;
esac
case ${CARGO_BUILD_TARGET:-} in
    '') ;;
    *) fail 'unset CARGO_BUILD_TARGET; this script packages native release binaries' ;;
esac
for command in cargo gzip install jq mktemp sha256sum strip tar; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done

METADATA=$(cargo metadata --manifest-path "$ROOT_DIR/Cargo.toml" --locked --no-deps --format-version 1)
EDITOR_VERSION=$(printf '%s' "$METADATA" | jq -er '.packages[] | select(.name == "ceditor") | .version')
PRODUCT_VERSION=$(printf '%s' "$METADATA" | jq -er '.packages[] | select(.name == "cagents") | .version')
VERSION=${VERSION:-$PRODUCT_VERSION}
[ "$VERSION" = "$EDITOR_VERSION" ] && [ "$VERSION" = "$PRODUCT_VERSION" ] \
    || fail 'release, cagents, and ceditor versions must match'
case $VERSION in
    ''|[!0-9]*|*..*|*[!0-9A-Za-z.+~-]*) fail "unsafe package version: $VERSION" ;;
esac

# Build the editor alone: combining workspace packages would unify host/UI features.
if [ "${CAGENTS_SKIP_BUILD:-0}" != 1 ]; then
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --locked --release \
        --target-dir "$TARGET_DIR" --package ceditor
fi
[ -x "$TARGET_DIR/release/ceditor" ] || fail "missing release binary: $TARGET_DIR/release/ceditor"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/ceditor-package.XXXXXX")
cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
TOP_LEVEL=ceditor-$VERSION
PACKAGE_ROOT=$WORK_DIR/$TOP_LEVEL
ARCHIVE_NAME=ceditor-$VERSION-$RELEASE_TARGET.tar.gz
install -d -m 0755 "$PACKAGE_ROOT/bin" "$PACKAGE_ROOT/share/applications" \
    "$PACKAGE_ROOT/share/icons/hicolor/scalable/apps" "$PACKAGE_ROOT/share/doc/ceditor" "$DIST_DIR"
install -m 0755 "$TARGET_DIR/release/ceditor" "$PACKAGE_ROOT/bin/ceditor"
strip --strip-unneeded "$PACKAGE_ROOT/bin/ceditor"
install -m 0644 "$ROOT_DIR/packaging/$DESKTOP_ID.desktop" "$PACKAGE_ROOT/share/applications/$DESKTOP_ID.desktop"
install -m 0644 "$ROOT_DIR/packaging/$DESKTOP_ID.svg" "$PACKAGE_ROOT/share/icons/hicolor/scalable/apps/$DESKTOP_ID.svg"
install -m 0644 "$ROOT_DIR/LICENSE" "$PACKAGE_ROOT/share/doc/ceditor/LICENSE"
(
    cd "$WORK_DIR"
    tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 \
        --numeric-owner --format=gnu -cf "$WORK_DIR/portable.tar" "$TOP_LEVEL"
)
gzip -n -9 -c "$WORK_DIR/portable.tar" > "$DIST_DIR/$ARCHIVE_NAME"
(
    cd "$DIST_DIR"
    sha256sum "$ARCHIVE_NAME" > SHA256SUMS-ceditor
)
printf 'Created standalone editor package:\n  %s\n  %s\n' \
    "$DIST_DIR/$ARCHIVE_NAME" "$DIST_DIR/SHA256SUMS-ceditor"
