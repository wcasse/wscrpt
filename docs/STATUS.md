# wscrpt status — SHIP / dual product lanes

**Date:** 2026-08-01  
**Goal:** open-source **v0.2.2** tonight  
**Publication tip:** `c3169f1` on `main` = tag **`v0.2.2`**  
**Prior tags:** `v0.2.0`, `v0.2.1` (never move)

## Active lanes (do not cross)

| Lane | Branch / tree | Owner | Handoff |
| --- | --- | --- | --- |
| **AGENTS** | `agents/w2-lane` · worktree `wscrpt-agents` | Agents Grok thread | [handoffs/AGENTS_LANE.md](handoffs/AGENTS_LANE.md) |
| **STICKIES** | recommend `stickies/*` · main tree OK | Stickies Grok thread | [handoffs/STICKIES_LANE.md](handoffs/STICKIES_LANE.md) |
| **SHIP** | packaging / publish | This ship thread | This file + [PUBLISH_RUNBOOK.md](PUBLISH_RUNBOOK.md) |

Full edit contract: **[LANES.md](LANES.md)**.

### Post-tag rule

- Feature work lands on lane branches, then `main` **after** `v0.2.2`.
- Do **not** move tag `v0.2.2`.
- Rebase lane branches onto `origin/main` (`c3169f1`) — history was rewritten for privacy (noreply + path fixtures).

## Shipped / gates

| Gate | Status |
| --- | --- |
| Snapshot audit | **Pass** on `c3169f1` |
| History audit | **Pass** (email + path rewrite force-with-lease) |
| Tag `v0.2.2` | On origin → `c3169f1` |
| `main` | Matches tag tip |
| Install from git tag | `cargo install --git … --tag v0.2.2 --locked` |
| GitHub Release pages | **Still blocked** — `gh` token invalid; paste `docs/releases/*.md` in UI |
| Topics / private vuln reporting | **Blocked** on `gh` / settings |
| crates.io | Dry-run when clean; publish needs `cargo login` |

## Remaining (human, ~5 min)

```sh
gh auth login -h github.com

gh release create v0.2.0 --title "wscrpt 0.2.0" --notes-file docs/releases/v0.2.0.md --verify-tag || true
gh release create v0.2.1 --title "wscrpt 0.2.1" --notes-file docs/releases/v0.2.1.md --verify-tag || true
gh release create v0.2.2 --title "wscrpt 0.2.2" --notes-file docs/releases/v0.2.2.md --verify-tag

gh repo edit wcasse/wscrpt \
  --add-topic terminal --add-topic editor --add-topic ide \
  --add-topic ssh --add-topic mosh --add-topic ipad --add-topic rust

# Settings → Code security → Private vulnerability reporting → Enable

cargo login
cargo publish --dry-run --locked
cargo publish --locked
```

Or paste release notes via GitHub UI if `gh` stays broken.

## Product in v0.2.2

| Item | Status |
| --- | --- |
| Remote-first editor core (0.2) | Shipped |
| First-edit cue / trusted Git / Stickies v1 / W0 | In 0.2.1 |
| Floating Stickies pad | In 0.2.2 |
| Agents dashboard + fake W2 + sticky brief | In 0.2.2 |
| Host-auth readiness (no secrets) | In 0.2.2 |
| Live ACP default | Not claimed — Agents post-0.2.2 |
| Sticky checklist fan-out (`Esc w C` / `Y`) | **Not in tag** — was README-only WIP; code parked in local stash for Stickies lane |

## Live log

- **2026-08-01 SHIP night:** History scrub (noreply + `/path/to/` fixtures) via filter-repo; `main` + `v0.2.2` → `c3169f1`. Concurrent lane WIP stashed on Groudon (`stash@{1}`). Incomplete post-tag README checklist commit dropped so tag stays honest.
- **Lanes:** rebase on rewritten `main` before next land. Local git `user.email` for this repo set to noreply.
