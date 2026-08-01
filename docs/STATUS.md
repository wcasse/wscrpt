# wscrpt status — SHIP / dual product lanes

**Date:** 2026-08-01  
**Goal:** open-source **v0.2.2** tonight  
**Local/remote tip (pre-packaging):** `0f2999d` on `main`  
**Prior tag:** `v0.2.1` → `c26d265` (keep; never move)  
**Target tag:** `v0.2.2` on freeze tip after packaging + gates

## Active lanes (do not cross)

| Lane | Branch / tree | Owner | Handoff |
| --- | --- | --- | --- |
| **AGENTS** | `agents/w2-lane` · worktree `wscrpt-agents` | Agents Grok thread | [handoffs/AGENTS_LANE.md](handoffs/AGENTS_LANE.md) |
| **STICKIES** | recommend `stickies/*` · main tree OK | Stickies Grok thread | [handoffs/STICKIES_LANE.md](handoffs/STICKIES_LANE.md) |
| **SHIP** | packaging on `main` (or `ship/v0.2.2`) | This ship thread | This file + [PUBLISH_RUNBOOK.md](PUBLISH_RUNBOOK.md) |

Full edit contract: **[LANES.md](LANES.md)**.

### SHIP freeze (tonight)

- After freeze: **only** SHIP packaging/docs/version commits on `main`.
- Feature work stays on lane branches for post-0.2.2.
- Product already on `main` for the cut: floating Stickies pad, Agents dashboard, W2 fake loop, host-auth readiness.

## Shipped / gates

| Gate | Status |
| --- | --- |
| Privacy history rewrite (earlier) | Done; local backup `backup/pre-privacy-rewrite-*` |
| Snapshot audit | **Was red** on absolute home paths in lane docs — SHIP scrubbing to fixtures |
| History audit | **Red** until 2 tip commits using personal author email are rewritten to noreply (owner force-with-lease) **or** accepted as intentional |
| Tag `v0.2.0` / `v0.2.1` | On origin (do not move) |
| Tag `v0.2.2` | **Not yet** |
| Install honesty | README updated to advertise `v0.2.2` once packaged |
| GitHub Release pages | **Blocked** — `gh` token invalid; use UI + `docs/releases/*.md` |
| Topics / private vuln reporting | **Blocked** on `gh` / settings |
| crates.io | **Blocked** — need `cargo login` / `CARGO_REGISTRY_TOKEN` |

## Remaining (human, ~5 min after packaging green)

```sh
# Once, for all future commits:
git config user.email '163216174+wcasse@users.noreply.github.com'

gh auth login -h github.com

# After SHIP packaging commit + verify + audit green:
git push origin main
git tag -a v0.2.2 -m "wscrpt 0.2.2"
git push origin v0.2.2

gh release create v0.2.0 --title "wscrpt 0.2.0" --notes-file docs/releases/v0.2.0.md --verify-tag || true
gh release create v0.2.1 --title "wscrpt 0.2.1" --notes-file docs/releases/v0.2.1.md --verify-tag || true
gh release create v0.2.2 --title "wscrpt 0.2.2" --notes-file docs/releases/v0.2.2.md --verify-tag

gh repo edit wcasse/wscrpt \
  --add-topic terminal --add-topic editor --add-topic ide \
  --add-topic ssh --add-topic mosh --add-topic ipad --add-topic rust

# GitHub → Settings → Code security → Private vulnerability reporting → Enable

cargo login
cargo publish --dry-run --locked
cargo publish --locked
```

## Product job list (historical)

| Item | Status |
| --- | --- |
| Native terminal/player workspace | Done (code; optional harness) |
| Public audit | In progress (path scrub + history email) |
| `wscrpt` on PATH | Done |
| Host macOS/Linux/WSL | Done |
| `Esc l` / `Esc L` | Done |
| First-edit cue / trusted Git / Stickies v1 | In 0.2.1 |
| Stickies floating pad | On main → 0.2.2 |
| Agents dashboard + fake W2 | On main → 0.2.2 |
| Live ACP default | Not claimed; Agents lane |

## Live log

- **2026-08-01 SHIP:** Third lane = packaging only. Cut **v0.2.2**. Fixed install honesty (tag lag). Scrubbing absolute paths from lane docs. History still has 2 gmail-authored tip commits on origin.
- **2026-08-01 lanes:** AGENTS vs STICKIES ownership in [LANES.md](LANES.md). Agents worktree `wscrpt-agents` / `agents/w2-lane`.
