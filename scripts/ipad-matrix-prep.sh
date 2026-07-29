#!/bin/sh
# Host-side prep for the iPad + Blink acceptance matrix.
# An agent (or you) can run this; only the human-on-iPad pass remains.
#
# Usage:
#   scripts/ipad-matrix-prep.sh [output-dir]
#
# Creates:
#   - a disposable Git fixture workspace
#   - ROUTE_RECORD.md partially filled from this host
#   - HUMAN_PASS.md with the short human-only checklist
#   - evidence of host gates (verify note + binary identity)

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPOSITORY_ROOT"

OUT_DIR=${1:-"$REPOSITORY_ROOT/scratch/ipad-matrix-$(date +%Y%m%d-%H%M%S)"}
mkdir -p "$OUT_DIR"
FIXTURE="$OUT_DIR/fixture"
RECORD="$OUT_DIR/ROUTE_RECORD.md"
HUMAN="$OUT_DIR/HUMAN_PASS.md"
NOTES="$OUT_DIR/HOST_NOTES.md"

resolve_binary() {
    if [ -n "${WSCRPT_BIN:-}" ] && [ -x "$WSCRPT_BIN" ]; then
        printf '%s\n' "$WSCRPT_BIN"
        return
    fi
    if [ -x "$REPOSITORY_ROOT/target/release/wscrpt" ]; then
        printf '%s\n' "$REPOSITORY_ROOT/target/release/wscrpt"
        return
    fi
    if command -v wscrpt >/dev/null 2>&1; then
        command -v wscrpt
        return
    fi
    echo "error: no wscrpt binary; build with: cargo build --release --locked" >&2
    echo "       or set WSCRPT_BIN=/path/to/wscrpt" >&2
    exit 1
}

BIN=$(resolve_binary)
BIN_SHA=$(shasum -a 256 "$BIN" | awk '{print $1}')
VERSION=$("$BIN" --version 2>/dev/null || true)
HEALTH=$("$BIN" --health 2>/dev/null || true)

# --- fixture workspace -------------------------------------------------------
rm -rf "$FIXTURE"
mkdir -p "$FIXTURE/src/nested" "$FIXTURE/docs" "$FIXTURE/.wscrpt" "$FIXTURE/ignored"

cat >"$FIXTURE/src/main.rs" <<'EOF'
fn main() {
    // TODO: matrix fixture
    println!("hello wscrpt — café 日本語 🎧");
    let long = "wrap-me-".repeat(40);
    println!("{long}");
}
EOF

cat >"$FIXTURE/src/nested/notes.md" <<'EOF'
# Notes

Line with tabs:	one	two	three
Combining: é (e + combining acute)
Emoji: 🚀 ✨
CRLF next is intentional in the sibling file.
EOF

# CRLF file
printf 'crlf line one\r\ncrlf line two with café\r\n' >"$FIXTURE/docs/crlf.txt"

# long mixed-width line for soft-wrap
python3 - <<'PY' >"$FIXTURE/docs/wide.txt"
print("ASCII-" + ("x" * 120))
print("混合幅" * 40)
print("tabs\there\tand\tmore\t" + ("y" * 80))
PY

printf 'not-text\0\1\2\3binary' >"$FIXTURE/docs/blob.bin"
echo 'ignored-secret' >"$FIXTURE/ignored/secret.txt"
printf 'target\nignored/\n' >"$FIXTURE/.gitignore"

cat >"$FIXTURE/.wscrpt/tasks.toml" <<'EOF'
version = 1

[tasks.hello]
argv = ["printf", "task-ok café 日本語\\nsrc/main.rs:2:5: error: fixture problem\\n"]

[tasks.hang-child]
# short sleep so stop can be exercised without hanging the suite forever
argv = ["sh", "-c", "echo start; sleep 8; echo end"]
EOF

(
    cd "$FIXTURE"
    git init -q -b main
    git config user.name "wscrpt-matrix"
    git config user.email "matrix@wscrpt.invalid"
    git add src docs .wscrpt .gitignore
    git commit -qm "fixture base"
    # modified
    echo "// modified" >>src/main.rs
    # staged
    echo "staged line" >docs/staged.txt
    git add docs/staged.txt
    # untracked
    echo "untracked" >docs/untracked.txt
)

# baseline hashes / git status for F01
(
    cd "$FIXTURE"
    find . -type f ! -path './.git/*' -print0 | sort -z | xargs -0 shasum -a 256
) >"$OUT_DIR/fixture-hashes-before.txt"
(
    cd "$FIXTURE"
    git status --porcelain=v2
    git rev-parse HEAD
) >"$OUT_DIR/fixture-git-before.txt"

# --- route record (host-filled) ---------------------------------------------
{
    echo "# Route record (host-prefilled)"
    echo
    echo "Fill the **iPad-side** rows on the device. Host rows are already filled."
    echo
    echo "| Field | Value |"
    echo "| --- | --- |"
    echo "| Device / keyboard | *(human)* |"
    echo "| iPadOS | *(human)* |"
    echo "| Blink version | *(human)* |"
    echo "| Transport (\`ssh\` or \`mosh\`) | *(human — prefer mosh)* |"
    echo "| Remote host / OS / architecture | $(uname -s) $(uname -m) · $(hostname) |"
    echo "| tmux version, socket/session, and active state | $(command -v tmux >/dev/null && tmux -V || echo missing) · *(record session on iPad)* |"
    echo "| \`TERM\`, \`COLORTERM\`, locale, \`SSH_*\`, \`MOSH_*\` | TERM=${TERM:-} COLORTERM=${COLORTERM:-} LANG=${LANG:-} LC_ALL=${LC_ALL:-} · *(recheck inside Blink session)* |"
    echo "| Source revision / checkout cleanliness | $(git -C "$REPOSITORY_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown) · $(git -C "$REPOSITORY_ROOT" status -sb 2>/dev/null | head -1) |"
    echo "| Installed binary path | \`$BIN\` |"
    echo "| Installed binary SHA-256 | \`$BIN_SHA\` |"
    echo "| \`wscrpt --version\` | \`$VERSION\` |"
    echo "| \`wscrpt --health\` | see HOST_NOTES.md |"
    echo "| Date / reviewer | $(date -u +%Y-%m-%d) · *(human)* |"
    echo
    echo "Confirm the binary launched from Blink is this path/hash (not \`/usr/bin/w\`)."
} >"$RECORD"

{
    echo "# Host notes"
    echo
    echo "## Binary"
    echo
    echo "path: $BIN"
    echo "sha256: $BIN_SHA"
    echo "version: $VERSION"
    echo
    echo "## --health"
    echo
    echo '```'
    printf '%s\n' "$HEALTH"
    echo '```'
    echo
    echo "## Host gates already covering functional risk"
    echo
    echo "These rows are **functionally** exercised by \`scripts/verify.sh\` / CI"
    echo "(Unix PTY + tmux smoke + unit tests). They do **not** need full human"
    echo "re-proof on host; only re-check on the real iPad route if something feels wrong."
    echo
    echo "| Matrix IDs | Covered by host automation |"
    echo "| --- | --- |"
    echo "| B03, B04 (unicode/wrap basics) | unix_pty soft-wrap + unicode tests |"
    echo "| C03 (in-buffer replace) | unit + integration editor tests |"
    echo "| E01/E02 task trust/stop (logic) | unit + PTY trusted_task test |"
    echo "| F01 git read-only (logic) | extensive git unit tests |"
    echo "| F02 shell handoff (logic) | unix_pty workspace_shell_handoff |"
    echo "| H02/H03 session/recovery (logic) | unit session/recovery tests |"
    echo "| H04 external change (logic) | unit save protection tests |"
    echo "| I01/I02 quit/cleanup (logic) | PTY cleanup + unit dirty quit |"
    echo
    echo "CI: https://github.com/wcasse/wscrpt/actions"
    echo "Fixture: $FIXTURE"
} >"$NOTES"

# --- short human pass -------------------------------------------------------
cat >"$HUMAN" <<EOF
# Human-only iPad pass (~15–25 min)

Run from **Blink → mosh/ssh → tmux → host**, in the fixture:

\`\`\`sh
cd "$FIXTURE"
$BIN .
\`\`\`

Mark each row PASS/FAIL. Agent-prepped evidence lives beside this file.

| ID | You do | Pass if | Result |
| --- | --- | --- | --- |
| H0 | Confirm binary: \`$BIN --version\` matches route record hash path | Not \`/usr/bin/w\` | |
| A01 | Launch in tmux over mosh | First frame usable; no freeze while Indexing/Git/Recovery may still show | |
| A02 | \`$BIN --input-diagnostics\`: delayed Esc→s, rapid Esc+s, Ctrl-K, Ctrl-G, arrows, paste, resize | Transcript sane; modes restore on exit | |
| B01 | Type fast in two files + backspace | Feels fine; no dropped/duped chars | |
| B02 | Esc, wait 3–5s, finish sequence; repeat Ctrl-K | No timeout; no command chars in buffer | |
| C01 | Quick Open + tree + one buffer switch | Responsive; opens intended files | |
| C02 | Project search + \`Esc w R\` after external edit (second shell) | Results usable; refresh works | |
| D01 | If LSP configured: completion once | Acceptable latency *(skip if no LS configured)* | |
| E01 | Task \`hello\`: refuse trust, then approve | No run before trust; problem line navigable | |
| F02 | \`Esc t t\`, run \`git status\`, exit shell | Clean return to editor | |
| G01 | Yank unicode + try clipboard | Register works; OSC52 ok or fails visibly | |
| H01 | Disconnect mosh, reconnect to same tmux | Continues; no key/redraw storm | |
| I02 | After quit, type in shell + \`stty -a\` | Normal echo; no stuck alt screen/mouse | |

## Verdicts (required)

- [ ] typing latency
- [ ] Escape/action delivery
- [ ] reconnect
- [ ] shell handoff return
- [ ] clean exit / terminal cleanup

Optional if time: B03/B04, C03, F01, H02–H04 — already host-proven; only re-check on route if something smells wrong.

## After pass

Copy this folder (or just HUMAN_PASS.md + ROUTE_RECORD.md) into:

\`dist/checkpoints/IPAD_$(date +%Y%m%d).md\`

or attach to the GitHub release notes.
EOF

echo "prepared: $OUT_DIR"
echo "fixture:  $FIXTURE"
echo "human:    $HUMAN"
echo "record:   $RECORD"
echo
echo "Next (human on iPad):"
echo "  1. mosh/ssh + tmux to this host"
echo "  2. open HUMAN_PASS.md checklist"
echo "  3. cd fixture && $BIN ."
