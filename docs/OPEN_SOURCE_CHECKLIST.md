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
- [x] Agent-friendly iPad prep (`scripts/ipad-matrix-prep.sh`)
- [x] Release notes draft (`docs/releases/v0.2.0.md`)

## Guaranteed improvements (no iPad required)

| Priority | Item | Why |
| --- | --- | --- |
| P0 | GitHub **Release** notes pages for `v0.2.0` and `v0.2.1` (drafts in `docs/releases/`) | Discoverability; install snippet; checksums |
| P0 | Enable **private vulnerability reporting** in repo settings | Matches SECURITY.md |
| P0 | `cargo publish --dry-run` then publish when ready | One-line install for strangers |
| P1 | Demo GIF / short terminal recording | First-impression for README |
| P1 | Delete or archive local `dist/checkpoints` binaries (210MB) after Release upload | Cleaner clones; already gitignored |
| P1 | Topics on GitHub: `terminal`, `editor`, `ide`, `ssh`, `mosh`, `ipad`, `rust` | Search |
| P2 | `authors` / contact already in Cargo.toml | crates.io metadata |
| P2 | First-hour help polish (`Esc ?` copy, README essentials) | Reduce “how do I…” issues |
| P2 | Optional: man page or extended `--help` examples | Power-user hygiene |

## Explicitly defer (not launch blockers)

- Full iPad matrix (human gate before *wide* confidence, not before “repo is public”)
- VS Code–parity features (rename/refactor, multi-file replace, Git write UI)
- Windows host support
- Large `app.rs` modularization (quality-of-life for contributors; do after launch noise)

## Wide-open definition

**Minimum for “public and findable”:** public repo + green CI + tag + install-from-git docs (done).

**Minimum for “confidently recommend to strangers”:** above + GitHub Release + crates.io + short human Blink pass filed.
