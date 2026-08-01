#!/bin/sh
set -eu

PUBLIC_AUDIT_SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PUBLIC_AUDIT_ROOT=$(CDPATH= cd -- "$PUBLIC_AUDIT_SCRIPT_DIR/.." && pwd)
cd "$PUBLIC_AUDIT_ROOT"

PUBLIC_AUDIT_MODE=${1:-snapshot}
if [ "$PUBLIC_AUDIT_MODE" != "snapshot" ] && [ "$PUBLIC_AUDIT_MODE" != "--history" ]; then
    echo "usage: $0 [--history]" >&2
    exit 2
fi

PUBLIC_AUDIT_TEMP=$(mktemp -d)
trap 'rm -rf "$PUBLIC_AUDIT_TEMP"' EXIT HUP INT TERM
PUBLIC_AUDIT_FAILED=0

public_audit_fail_file() {
    PUBLIC_AUDIT_LABEL=$1
    PUBLIC_AUDIT_FILE=$2
    if [ -s "$PUBLIC_AUDIT_FILE" ]; then
        echo "FAIL: $PUBLIC_AUDIT_LABEL" >&2
        sed -n '1,40p' "$PUBLIC_AUDIT_FILE" >&2
        PUBLIC_AUDIT_FAILED=1
    fi
}

PUBLIC_AUDIT_PATH_PATTERN='(/Users/[^/[:space:]`"]+/|/home/[^/[:space:]`"]+/|[A-Za-z]:\\Users\\[^\\[:space:]`"]+\\)'
PUBLIC_AUDIT_SECRET_PATTERN='-----BEGIN (OPENSSH|RSA|EC|DSA|PGP) PRIVATE KEY-----|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}'
PUBLIC_AUDIT_EMAIL_PATTERN='[[:alnum:]._%+-]+@[[:alnum:].-]+\.[[:alpha:]]{2,}'

git grep -n -I -E -e "$PUBLIC_AUDIT_PATH_PATTERN" -- . \
    ':(exclude)scripts/audit-public-source.sh' >"$PUBLIC_AUDIT_TEMP/paths" || true
public_audit_fail_file "developer-specific absolute home path in tracked snapshot" "$PUBLIC_AUDIT_TEMP/paths"

git grep -n -I -E -e "$PUBLIC_AUDIT_SECRET_PATTERN" -- . \
    ':(exclude)scripts/audit-public-source.sh' >"$PUBLIC_AUDIT_TEMP/secrets" || true
public_audit_fail_file "recognizable secret material in tracked snapshot" "$PUBLIC_AUDIT_TEMP/secrets"

git grep -n -I -E -e "$PUBLIC_AUDIT_EMAIL_PATTERN" -- . \
    ':(exclude)scripts/audit-public-source.sh' >"$PUBLIC_AUDIT_TEMP/emails-all" || true
grep -Ev '@([[:alnum:]-]+\.)*(invalid|example\.com)([^[:alnum:].-]|$)|@users\.noreply\.github\.com([^[:alnum:].-]|$)' \
    "$PUBLIC_AUDIT_TEMP/emails-all" >"$PUBLIC_AUDIT_TEMP/emails" || true
public_audit_fail_file "non-fixture personal email in tracked snapshot" "$PUBLIC_AUDIT_TEMP/emails"

git ls-files | grep -E '(^|/)(\.env($|\.)|id_(rsa|dsa|ecdsa|ed25519)($|\.)|[^/]+\.(pem|p12|pfx|key|mobileprovision)$)' \
    >"$PUBLIC_AUDIT_TEMP/credential-files" || true
public_audit_fail_file "credential-shaped file is tracked" "$PUBLIC_AUDIT_TEMP/credential-files"

git ls-files | grep -E '(^|/)(target|node_modules|DerivedData|xcuserdata)/' \
    >"$PUBLIC_AUDIT_TEMP/generated" || true
public_audit_fail_file "generated dependency/build directory is tracked" "$PUBLIC_AUDIT_TEMP/generated"

git ls-files >"$PUBLIC_AUDIT_TEMP/tracked"
while IFS= read -r PUBLIC_AUDIT_TRACKED_FILE; do
    [ -f "$PUBLIC_AUDIT_TRACKED_FILE" ] || continue
    PUBLIC_AUDIT_BYTES=$(wc -c <"$PUBLIC_AUDIT_TRACKED_FILE" | tr -d ' ')
    if [ "$PUBLIC_AUDIT_BYTES" -gt 5242880 ]; then
        printf '%s\t%s bytes\n' "$PUBLIC_AUDIT_TRACKED_FILE" "$PUBLIC_AUDIT_BYTES" \
            >>"$PUBLIC_AUDIT_TEMP/large"
    fi
done <"$PUBLIC_AUDIT_TEMP/tracked"
public_audit_fail_file "tracked file exceeds five-megabyte review threshold" "$PUBLIC_AUDIT_TEMP/large"

for PUBLIC_AUDIT_REQUIRED in LICENSE THIRD_PARTY_NOTICES.md Cargo.lock previewd/package-lock.json \
    clients/ipad-preview-harness/PreviewHarness.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved; do
    if [ ! -f "$PUBLIC_AUDIT_REQUIRED" ]; then
        echo "$PUBLIC_AUDIT_REQUIRED" >>"$PUBLIC_AUDIT_TEMP/missing"
    fi
done
public_audit_fail_file "required license or dependency-lock evidence is missing" "$PUBLIC_AUDIT_TEMP/missing"

if [ "$PUBLIC_AUDIT_MODE" = "--history" ]; then
    git log HEAD --format='%ae%n%ce' | sort -u \
        | grep -Ev '@([[:alnum:]-]+\.)*invalid$|@users\.noreply\.github\.com$' \
        >"$PUBLIC_AUDIT_TEMP/history-emails" || true
    if [ -s "$PUBLIC_AUDIT_TEMP/history-emails" ]; then
        PUBLIC_AUDIT_HISTORY_EMAIL_COUNT=$(wc -l <"$PUBLIC_AUDIT_TEMP/history-emails" | tr -d ' ')
        echo "${PUBLIC_AUDIT_HISTORY_EMAIL_COUNT} non-fixture author/committer address(es) in reachable history" \
            >"$PUBLIC_AUDIT_TEMP/history-email-summary"
    fi
    public_audit_fail_file "reachable commit metadata contains personal identity" "$PUBLIC_AUDIT_TEMP/history-email-summary"

    for PUBLIC_AUDIT_REVISION in $(git rev-list HEAD); do
        if git grep -I -q -E -e "$PUBLIC_AUDIT_PATH_PATTERN" \
            "$PUBLIC_AUDIT_REVISION" -- . \
            ':(exclude)scripts/audit-public-source.sh' 2>/dev/null; then
            echo "$PUBLIC_AUDIT_REVISION" >>"$PUBLIC_AUDIT_TEMP/history-paths"
        fi
        if git grep -I -q -E -e "$PUBLIC_AUDIT_SECRET_PATTERN" \
            "$PUBLIC_AUDIT_REVISION" -- . \
            ':(exclude)scripts/audit-public-source.sh' 2>/dev/null; then
            echo "$PUBLIC_AUDIT_REVISION" >>"$PUBLIC_AUDIT_TEMP/history-secrets"
        fi
    done
    public_audit_fail_file "reachable historical snapshots contain developer-specific absolute home paths" "$PUBLIC_AUDIT_TEMP/history-paths"
    public_audit_fail_file "reachable historical snapshots contain recognizable secret material" "$PUBLIC_AUDIT_TEMP/history-secrets"
fi

if [ "$PUBLIC_AUDIT_FAILED" -ne 0 ]; then
    exit 1
fi

echo "public source audit passed ($PUBLIC_AUDIT_MODE)"
