# wscrpt status — ship readiness

**Date:** 2026-08-01  
**Install tag (strangers):** **`v0.2.2`** → `c3169f1`  
**`main` tip:** may be ahead of the tag (post-tag feature landings are OK; do not move tags)

## Ready-to-ship scoreboard

### Already done (ship-capable without `gh`)

| Gate | Status |
| --- | --- |
| Public repo, MIT, issue/PR templates, CODEOWNERS | Done |
| Privacy scrub (noreply + path fixtures) | Done; snapshot + **history** audit pass |
| Tag **`v0.2.2`** on origin | Done — never move |
| README install → `v0.2.2` | Done |
| `scripts/verify.sh` on packaging tip | Green (local) |
| Install from public tag | Verified: `cargo install --git … --tag v0.2.2 --locked` |
| `cargo publish --dry-run --locked` | Green (needs `cargo login` to actually publish) |
| SSH push to GitHub | Working (this is enough for code + tags) |

### When you’re home (~5 min, needs browser / tokens)

| Gate | Why it matters | How |
| --- | --- | --- |
| **GitHub Release pages** for 0.2.0 / 0.2.1 / **0.2.2** | Discoverability; nice landing; not required to install from tag | UI: Releases → draft from tag, paste `docs/releases/v0.2.x.md` **or** `gh auth login` then runbook |
| **Topics** | Search: `terminal editor ide ssh mosh ipad rust` | Repo settings or `gh repo edit … --add-topic` |
| **Private vulnerability reporting** | SECURITY.md points here | Settings → Code security → enable |
| **crates.io publish** | `cargo install wscrpt` for people who never clone | `cargo login` then `cargo publish --locked` |

`gh` CLI auth is **optional**. SSH already covers push/tag. Releases can be pure GitHub UI.

### Not ship blockers (honest product)

| Item | Note |
| --- | --- |
| Full iPad / Blink matrix | Human confidence gate; prep script exists |
| Demo GIF | `docs/demo.tape` ready; needs `vhs` |
| Live ACP as default | Agents lane post-0.2.2 |
| Sticky checklist fan-out on **`main`** | Landed **after** tag as `e93f934+`; install tag does **not** include it until **v0.2.3** |
| `app.rs` modularization | Contributor QoL later |
| Local stashes on Groudon | Lane WIP park — not for remote |

## Lanes

| Lane | Owns | Note |
| --- | --- | --- |
| **AGENTS** | `agent*.rs`, dashboard, ACP | Worktree `wscrpt-agents` — rebase on rewritten `main` |
| **STICKIES** | pad, checklist fan-out | Prefer `stickies/*` branch for post-tag work |
| **SHIP** | tags, audits, release notes, publish | Freeze product into new tags only |

Contract: [LANES.md](LANES.md).

## Home script (copy-paste when tokens work)

```sh
# Optional — UI works without this
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

## Definition of “ready”

| Bar | Met when |
| --- | --- |
| **A — Public installable (met)** | Public repo + green verify culture + tag + install docs + clean audits |
| **B — Confidently recommend** | A + Release page + crates.io + (optional) short Blink pass |
| **C — Greatest remote IDE forever** | B + ongoing Agents/Stickies + iPad matrix + demo |

Tonight’s code ship is **bar A**. Bar B is the home errand. Bar C is the product roadmap.

## Live log

- **2026-08-01:** `v0.2.2` / `c3169f1` public; history scrubbed; SSH push fine; Releases=0, topics=[], no crates.io yet. Post-tag checklist fan-out on `main` — cut **v0.2.3** only if install should include it.
