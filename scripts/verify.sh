#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPOSITORY_ROOT"

echo "==> host"
uname -a
rustc -Vv
cargo -V
if command -v tmux >/dev/null 2>&1; then
    tmux -V
else
    echo "tmux: unavailable"
fi

echo "==> public source audit"
"$SCRIPT_DIR/audit-public-source.sh"

echo "==> formatting"
cargo fmt --all -- --check

echo "==> clippy"
cargo clippy --all-targets --all-features --locked -- -D warnings

echo "==> all targets"
cargo test --all-targets --all-features --locked -- --nocapture

echo "==> documentation"
cargo test --doc --all-features --locked

echo "==> package"
PACKAGE_VERSION=$(cargo pkgid --locked | sed -e 's/.*#//' -e 's/.*@//')
echo "package version: $PACKAGE_VERSION"
cargo package --locked --allow-dirty

VERIFY_TEMP=$(mktemp -d)
trap 'rm -rf "$VERIFY_TEMP"' EXIT HUP INT TERM
mkdir -p "$VERIFY_TEMP/source"
PACKAGE="$REPOSITORY_ROOT/target/package/wscrpt-$PACKAGE_VERSION.crate"
shasum -a 256 "$PACKAGE"
tar -xzf "$PACKAGE" -C "$VERIFY_TEMP/source"

echo "==> isolated install from packaged source"
cargo install \
    --path "$VERIFY_TEMP/source/wscrpt-$PACKAGE_VERSION" \
    --root "$VERIFY_TEMP/install" \
    --locked

INSTALLED_WSCRPT="$VERIFY_TEMP/install/bin/wscrpt"
"$INSTALLED_WSCRPT" --version
"$INSTALLED_WSCRPT" --health
"$INSTALLED_WSCRPT" --print-default-config >/dev/null
"$INSTALLED_WSCRPT" --print-command-reference >/dev/null
shasum -a 256 "$INSTALLED_WSCRPT"

echo "verification complete: $($INSTALLED_WSCRPT --version)"
