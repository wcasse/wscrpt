# wscrpt dual-thread status

**Date:** 2026-08-01  
**Goal:** open-source ready today  
**Local/remote tip:** `c26d265` on `main` (privacy-rewritten history)  
**Tag:** `v0.2.1` → `c26d265`

## Shipped

| Gate | Status |
| --- | --- |
| Privacy history rewrite | Done; local backup `backup/pre-privacy-rewrite-*` |
| Snapshot audit | Pass |
| History audit | Pass |
| `main` pushed | Yes (force-with-lease after rewrite) |
| Tag `v0.2.0` | On rewritten history on origin |
| Tag `v0.2.1` | On origin → publication tip |
| Install from git tag | `cargo install --git … --tag v0.2.1 --locked` verified |
| GitHub Release pages | **Blocked** — `gh` token invalid; use UI + `docs/releases/*.md` |
| Topics | **Blocked** on `gh` |
| Private vuln reporting | **Blocked** on settings/`gh` |
| crates.io | **Blocked** — need `cargo login` / `CARGO_REGISTRY_TOKEN` |

## Remaining (human, ~5 min)

```sh
gh auth login -h github.com

gh release create v0.2.0 --title "wscrpt 0.2.0" --notes-file docs/releases/v0.2.0.md --verify-tag
gh release create v0.2.1 --title "wscrpt 0.2.1" --notes-file docs/releases/v0.2.1.md --verify-tag

gh repo edit wcasse/wscrpt \
  --add-topic terminal --add-topic editor --add-topic ide \
  --add-topic ssh --add-topic mosh --add-topic ipad --add-topic rust

# GitHub → Settings → Code security → Private vulnerability reporting → Enable

cargo login
cargo publish --dry-run --locked
cargo publish --locked
```

Or paste release notes via GitHub UI if `gh` stays broken.

## Product job list

| Item | Status |
| --- | --- |
| Native terminal/player workspace | Done (code) |
| Public audit | Done |
| `wscrpt` on PATH | Done |
| Host macOS/Linux/WSL | Done |
| `Esc l` / `Esc L` | Done |
| First-edit cue / trusted Git / Stickies / W0 | In 0.2.1 |

## Live log

- **Ship:** force-pushed rewritten `main` to `c26d265`, published tags `v0.2.0` + `v0.2.1`. `gh`/crates.io still need credentials.
