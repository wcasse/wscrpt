# Open-source readiness checklist

Not blocked on the iPad matrix. Use this while the hardware pass waits.

## Done

- [x] Public repo: https://github.com/wcasse/wscrpt
- [x] MIT license
- [x] Single product/binary name `wscrpt`
- [x] CI: stable macOS + Linux + MSRV 1.88
- [x] Tag `v0.2.0` / `v0.2.1` — **pushed tags never move.** Fixes ship as new patch versions.
- [x] CONTRIBUTING / SECURITY / issue templates
- [x] Contributor map (`docs/CONTRIBUTOR_MAP.md`)
- [x] Host verify gate (`scripts/verify.sh`)
- [x] Dependency license inventory (`THIRD_PARTY_NOTICES.md`)
- [x] Agent-friendly iPad prep (`scripts/ipad-matrix-prep.sh`)
- [x] Release notes drafts (`docs/releases/v0.2.0.md`, `v0.2.1.md`, `v0.2.2.md`)
- [x] Dual-lane ownership contract (`docs/LANES.md`)

## Guaranteed improvements (no iPad required)

Live coordination: [`docs/STATUS.md`](STATUS.md).

| Priority | Item | Why | Live status (2026-08-01 evening) |
| --- | --- | --- | --- |
| P0 | Snapshot audit green on freeze tip | No home paths / secrets | SHIP scrubbing lane docs; re-run before tag |
| P0 | History audit green **or** owner-accepted rewrite | Personal author email on 2 tip commits | Needs noreply rewrite + force-with-lease **or** explicit accept |
| P0 | Package **v0.2.2** (Cargo.toml + CHANGELOG + README install) | Install honesty: main had pad/dashboard; tag lagged | Packaging in progress |
| P0 | GitHub **Release** notes for `v0.2.0`–`v0.2.2` | Discoverability | Drafts ready; pages blocked on `gh` auth |
| P0 | Enable **private vulnerability reporting** | Matches SECURITY.md | Blocked on settings / `gh` |
| P0 | `cargo publish --dry-run` then publish | One-line install for strangers | Pending freeze tip |
| P1 | Demo GIF | First impression | `docs/demo.tape` present; `vhs` not on PATH tonight |
| P1 | Topics: `terminal`, `editor`, `ide`, `ssh`, `mosh`, `ipad`, `rust` | Search | Blocked on `gh` |
| P1 | Local `dist/checkpoints` binaries (210MB) after Release upload | Cleaner disk; already gitignored | Safe anytime |
| P2 | Durable private maintainer route without personal email in package metadata | Security routing | Package tip sanitized; SECURITY routing present |

## Explicitly defer (not launch blockers)

- Full iPad matrix (human gate before *wide* confidence, not before “repo is public”)
- Live ACP as default / Needs You / worktree isolation (Agents post-0.2.2)
- Stickies multi-card native geometry (Stickies / native client)
- VS Code–parity features (rename/refactor, multi-file replace, full Git client)
- Native Windows host support beyond the documented WSL route
- Large `app.rs` modularization

## Wide-open definition

**Minimum for “public and findable”:** public repo + green CI + tag + install-from-git docs.

**Minimum for “confidently recommend to strangers” (tonight’s bar):** above + **v0.2.2** tag matching product tip + GitHub Release + crates.io when credentials allow + short human Blink pass when hardware is free.

Run `scripts/audit-public-source.sh` before each publication. Its default mode
checks the current tracked snapshot. `scripts/audit-public-source.sh --history`
also checks reachable history and is intentionally a separate gate because
history cleanup would require an explicit owner decision and coordinated rewrite.
