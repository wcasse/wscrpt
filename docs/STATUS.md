# wscrpt status — ship readiness

**Date:** 2026-08-01  
**Install tag (strangers):** **`v0.2.3`** → `4d25883`  
**Prior tags:** `v0.2.0`, `v0.2.1`, `v0.2.2` — **never move**

## Ready-to-ship scoreboard

### Code / install

| Gate | Status |
| --- | --- |
| Public repo, MIT, CI | Done |
| Privacy / path history scrub | **Pass** (snapshot + history) after force-with-lease rewrite |
| Tag **`v0.2.3`** | On origin → `4d25883` |
| README install → `v0.2.3` | Done |
| Install from public tag | Verified `cargo install --git … --tag v0.2.3 --locked` |
| Sticky paste fix, S4 `Esc w A`, checklist, pad UX polish | In 0.2.3 body |

### Home (~5 min, tokens)

| Gate | How |
| --- | --- |
| GitHub Release pages 0.2.0–0.2.3 | UI or `gh` + `docs/releases/` |
| Topics | `terminal editor ide ssh mosh ipad rust` |
| Private vulnerability reporting | Repo settings |
| crates.io | `cargo login` + `cargo publish --locked` |

## Product in v0.2.3

Pad + dashboard + sticky brief + checklist fan-out + receipt log write-back + paste-to-pad fix. Agents still fake-by-default.

## Live log

- **2026-08-01 night:** Cut **v0.2.3**; history scrub residual `/path/to/...` fixtures in launch design commit.
