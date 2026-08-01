# Open-source readiness checklist

Not blocked on the iPad matrix. Use this while the hardware pass waits.

## Done

- [x] Public repo: https://github.com/wcasse/wscrpt
- [x] MIT license
- [x] Single product/binary name `wscrpt`
- [x] CI: stable macOS + Linux + MSRV 1.88
- [x] Tag `v0.2.0` — **pushed tags never move.** Fixes after a public tag ship as a new patch version (`v0.2.1`, …) so existing clones stay reproducible.
- [x] CONTRIBUTING / SECURITY / issue templates
- [x] Contributor map (`docs/CONTRIBUTOR_MAP.md`)
- [x] Host verify gate (`scripts/verify.sh`)
- [x] Current-snapshot privacy/secret/package audit (`scripts/audit-public-source.sh`)
- [x] Dependency license inventory (`THIRD_PARTY_NOTICES.md`)
- [x] Agent-friendly iPad prep (`scripts/ipad-matrix-prep.sh`)
- [x] Release notes draft (`docs/releases/v0.2.0.md`)

## Guaranteed improvements (no iPad required)

Live coordination: [`docs/STATUS.md`](STATUS.md). Snapshot audit re-run on 2026-08-01: **pass**.

| Priority | Item | Why | Live status (2026-08-01) |
| --- | --- | --- | --- |
| P0 | GitHub **Release** notes pages for `v0.2.0` and `v0.2.1` (drafts in `docs/releases/`) | Discoverability; install snippet; checksums | Drafts ready (0.2.1 folded for Strategy A). **Pages not created.** Tag `v0.2.1` missing. See `docs/PUBLISH_RUNBOOK.md`. |
| P0 | Enable **private vulnerability reporting** in repo settings | Matches SECURITY.md | Blocked on working GitHub auth / settings UI |
| P0 | `cargo publish --dry-run` then publish when ready | One-line install for strangers | Dry-run pending this pass; crates.io not published |
| P0 | Push local `main` (11 commits ahead of origin) | Public tip is still `905d010`; README already advertises `v0.2.1` | Human push after verify + `gh auth login` |
| P0 | Decide whether existing commit identities/history are intentionally public; see `docs/PUBLIC_SOURCE_AUDIT.md` | A clean tip does not remove data from reachable history | **Recommend intentional public** (owner identity already on repo); no rewrite |
| P1 | Demo GIF / short terminal recording | First-impression for README | `docs/demo.tape` present; GIF optional |
| P1 | Delete or archive local `dist/checkpoints` binaries (210MB) after Release upload | Cleaner clones; already gitignored | Local-only; safe anytime |
| P1 | Topics on GitHub: `terminal`, `editor`, `ide`, `ssh`, `mosh`, `ipad`, `rust` | Search | Blocked on `gh` token |
| P2 | Publish a durable private maintainer/contact route without embedding a personal email in package metadata | Security and conduct routing | Package tip sanitized; SECURITY routing present |
| P2 | First-hour help polish (`Esc ?` copy, README essentials) | Reduce “how do I…” issues | First-run help shipped in 0.2.1 body |
| P2 | Optional: man page or extended `--help` examples | Power-user hygiene | Deferred |

## Explicitly defer (not launch blockers)

- Full iPad matrix (human gate before *wide* confidence, not before “repo is public”)
- VS Code–parity features (rename/refactor, multi-file replace, Git write UI)
- Native Windows host support beyond the documented WSL route
- Large `app.rs` modularization (quality-of-life for contributors; do after launch noise)

## Wide-open definition

**Minimum for “public and findable”:** public repo + green CI + tag + install-from-git docs (done).

**Minimum for “confidently recommend to strangers”:** above + GitHub Release + crates.io + short human Blink pass filed.

Run `scripts/audit-public-source.sh` before each publication. Its default mode
checks the current tracked snapshot. `scripts/audit-public-source.sh --history`
also checks reachable history and is intentionally a separate gate because
history cleanup would require an explicit owner decision and coordinated rewrite.
