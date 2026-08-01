#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
PREVIEW_ROOT="$REPOSITORY_ROOT/previewd"

if ! command -v node >/dev/null 2>&1; then
    echo "node 22 or newer is required" >&2
    exit 1
fi

NODE_VERSION=$(node -p "process.versions.node")
NODE_MAJOR=$(node -p "Number(process.versions.node.split('.')[0])")
if [ "$NODE_MAJOR" -lt 22 ]; then
    echo "node 22 or newer is required; found $NODE_VERSION" >&2
    exit 1
fi

echo "==> preview host"
echo "node $NODE_VERSION"
npm --version

echo "==> locked preview dependencies"
if [ "${WSCRPT_PREVIEW_SKIP_INSTALL:-0}" != "1" ]; then
    npm --prefix "$PREVIEW_ROOT" ci --ignore-scripts --no-audit --no-fund
fi

echo "==> preview unit and security tests"
# The E2E file is part of the test glob but environment-gated. Keep this lane
# serialized: running two headless browsers alongside the unit pool distorts
# the cadence measurement and then duplicates the explicit smoke below.
WSCRPT_PREVIEW_CHROME= npm --prefix "$PREVIEW_ROOT" test

if [ -n "${WSCRPT_PREVIEW_CHROME:-}" ]; then
    echo "==> local Chromium functional smoke"
    npm --prefix "$PREVIEW_ROOT" run test:e2e
else
    echo "local Chromium smoke: skipped (set WSCRPT_PREVIEW_CHROME)"
fi

if [ "${WSCRPT_PREVIEW_SKIP_XCODE:-0}" = "1" ]; then
    echo "iPad harness compile: skipped (WSCRPT_PREVIEW_SKIP_XCODE=1)"
elif command -v xcodebuild >/dev/null 2>&1 && \
    [ -d "$REPOSITORY_ROOT/clients/ipad-preview-harness/PreviewHarness.xcodeproj" ]; then
    VERIFY_PREVIEW_TEMP=$(mktemp -d)
    trap 'rm -rf "$VERIFY_PREVIEW_TEMP"' EXIT HUP INT TERM
    echo "==> locked native iPad dependencies"
    xcodebuild \
        -resolvePackageDependencies \
        -project "$REPOSITORY_ROOT/clients/ipad-preview-harness/PreviewHarness.xcodeproj" \
        -scheme PreviewHarness \
        -onlyUsePackageVersionsFromResolvedFile \
        -clonedSourcePackagesDirPath "$VERIFY_PREVIEW_TEMP/SourcePackages"

    echo "==> native iPad terminal/player compile"
    xcodebuild \
        -project "$REPOSITORY_ROOT/clients/ipad-preview-harness/PreviewHarness.xcodeproj" \
        -scheme PreviewHarness \
        -configuration Debug \
        -sdk iphoneos \
        -destination 'generic/platform=iOS' \
        -onlyUsePackageVersionsFromResolvedFile \
        -clonedSourcePackagesDirPath "$VERIFY_PREVIEW_TEMP/SourcePackages" \
        -disableAutomaticPackageResolution \
        -derivedDataPath "$VERIFY_PREVIEW_TEMP/DerivedData" \
        CODE_SIGNING_ALLOWED=NO \
        build-for-testing

    echo "==> optimized native iPad terminal/player compile"
    xcodebuild \
        -project "$REPOSITORY_ROOT/clients/ipad-preview-harness/PreviewHarness.xcodeproj" \
        -scheme PreviewHarness \
        -configuration Release \
        -sdk iphoneos \
        -destination 'generic/platform=iOS' \
        -onlyUsePackageVersionsFromResolvedFile \
        -clonedSourcePackagesDirPath "$VERIFY_PREVIEW_TEMP/SourcePackages" \
        -disableAutomaticPackageResolution \
        -derivedDataPath "$VERIFY_PREVIEW_TEMP/ReleaseDerivedData" \
        CODE_SIGNING_ALLOWED=NO \
        build
else
    echo "native iPad terminal/player compile: skipped (xcodebuild/project unavailable)"
fi

echo "preview Phase 0 local verification complete"
