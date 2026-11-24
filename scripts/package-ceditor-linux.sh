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
STAGE=
PACKAGE_STAGE=
REUSE_COMPANIONS=

fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --help)
            [ "$#" -eq 1 ] || fail 'usage: package-ceditor-linux.sh [--help]'
            printf '%s\n' 'Build a standalone ceditor Linux archive with its desktop entry and icon.' \
                'Environment: VERSION, CAGENTS_RELEASE_TARGET, CAGENTS_DIST_DIR, CARGO_TARGET_DIR,' \
                'SOURCE_DATE_EPOCH, CAGENTS_SKIP_BUILD=1 (package an existing release binary).'
            exit 0
            ;;
        --stage|--package-stage|--reuse-companions)
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a directory"
            case $1 in
                --stage) STAGE=$2 ;;
                --package-stage) PACKAGE_STAGE=$2 ;;
                --reuse-companions) REUSE_COMPANIONS=$2 ;;
            esac
            shift 2
            ;;
        *) fail 'usage: package-ceditor-linux.sh [--help]' ;;
    esac
done
if [ -n "$STAGE$PACKAGE_STAGE" ]; then
    [ "${CAGENTS_SKIP_BUILD:-0}" != 1 ] || fail 'invalid staged release options'
    [ -z "$STAGE" ] || [ -z "$PACKAGE_STAGE" ] || fail 'select one release stage'
fi
if [ -n "$REUSE_COMPANIONS" ]; then
    [ -n "$STAGE" ] && [ -x "$REUSE_COMPANIONS/cagents" ] &&
        [ -x "$REUSE_COMPANIONS/cagentsd" ] ||
        fail 'reusing companions requires an editor build stage and both executables'
fi

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

# Build each implementation graph separately; never unify host/UI features.
if [ -n "$PACKAGE_STAGE" ]; then
    CLI_BINARY=$PACKAGE_STAGE/cagents
    HOST_BINARY=$PACKAGE_STAGE/cagentsd
    EDITOR_BINARY=$PACKAGE_STAGE/ceditor
elif [ "${CAGENTS_SKIP_BUILD:-0}" != 1 ]; then
    [ -x /usr/bin/python3 ] || fail 'required command not found: /usr/bin/python3'
    # The package is native-only. Override config build.target with the validated
    # release target, and consume each exact Cargo executable instead of guessing
    # whether it lives below a target-triple directory.
    if [ -n "$REUSE_COMPANIONS" ]; then
        CLI_BINARY=$REUSE_COMPANIONS/cagents
        HOST_BINARY=$REUSE_COMPANIONS/cagentsd
    else
        CLI_BINARY=$(/usr/bin/python3 "$SCRIPT_DIR/build-ceditor-product" cagents \
            --manifest-path "$ROOT_DIR/Cargo.toml" --locked --release \
            --target-dir "$TARGET_DIR" --target "$RELEASE_TARGET" --no-default-features --bin cagents)
        HOST_BINARY=$(/usr/bin/python3 "$SCRIPT_DIR/build-ceditor-product" cagentsd \
            --manifest-path "$ROOT_DIR/Cargo.toml" --locked --release \
            --target-dir "$TARGET_DIR" --target "$RELEASE_TARGET" --no-default-features --features host --bin cagentsd)
    fi
    EDITOR_BINARY=$(/usr/bin/python3 "$SCRIPT_DIR/build-ceditor-product" ceditor \
        --manifest-path "$ROOT_DIR/Cargo.toml" --locked --release \
        --target-dir "$TARGET_DIR" --target "$RELEASE_TARGET" --package ceditor)
else
    # Explicit prebuilt mode retains its documented native release layout.
    CLI_BINARY=$TARGET_DIR/release/cagents
    HOST_BINARY=$TARGET_DIR/release/cagentsd
    EDITOR_BINARY=$TARGET_DIR/release/ceditor
fi
for binary in "$EDITOR_BINARY" "$CLI_BINARY" "$HOST_BINARY"; do
    [ -x "$binary" ] || fail "missing release binary: $binary"
done
if [ -n "$STAGE" ]; then
    mkdir -- "$STAGE"
    cp --reflink=auto -- "$CLI_BINARY" "$STAGE/cagents"
    cp --reflink=auto -- "$HOST_BINARY" "$STAGE/cagentsd"
    cp --reflink=auto -- "$EDITOR_BINARY" "$STAGE/ceditor"
    exit 0
fi
if [ "${CAGENTS_SKIP_BUILD:-0}" = 1 ] || [ -n "$PACKAGE_STAGE" ]; then
    require_prebuilt_version() {
        # Preserve output through command substitution so extra lines cannot
        # disappear. Accept the exact line with or without its final newline.
        reported=$("$2" --version && printf '.') \
            || fail "could not verify prebuilt $1 version: $2"
        expected="$1 $VERSION"
        [ "$reported" = "$expected." ] \
            || [ "$reported" = "$(printf '%s\n.' "$expected")" ] \
            || fail "prebuilt $1 does not report requested version $VERSION: $2"
    }
    require_prebuilt_version ceditor "$EDITOR_BINARY"
    require_prebuilt_version cagents "$CLI_BINARY"
    require_prebuilt_version cagentsd "$HOST_BINARY"
fi

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
PRIVATE=$PACKAGE_ROOT/lib/ceditor
install -d -m 0755 "$PRIVATE/releases/$VERSION/bin"
install -m 0755 "$EDITOR_BINARY" "$PRIVATE/releases/$VERSION/bin/ceditor"
install -m 0755 "$CLI_BINARY" "$PRIVATE/releases/$VERSION/bin/cagents"
install -m 0755 "$HOST_BINARY" "$PRIVATE/releases/$VERSION/bin/cagentsd"
for binary in ceditor cagents cagentsd; do
    strip --strip-unneeded "$PRIVATE/releases/$VERSION/bin/$binary"
done
printf '%s\n' 'ceditor companions v1' > "$PRIVATE/releases/$VERSION/bin/.ceditor-companions"
chmod 644 "$PRIVATE/releases/$VERSION/bin/.ceditor-companions"
ln -s "releases/$VERSION" "$PRIVATE/current"
ln -s ../lib/ceditor/current/bin/ceditor "$PACKAGE_ROOT/bin/ceditor"
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
