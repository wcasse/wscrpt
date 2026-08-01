# wscrpt status — ship readiness

**Date:** 2026-08-01  
**Install tag (strangers):** **`v0.2.3`** (after this cut)  
**Prior tags:** `v0.2.0`, `v0.2.1`, `v0.2.2` — **never move**

## Ready-to-ship scoreboard

### Code / install

| Gate | Status |
| --- | --- |
| Public repo, MIT, CI | Done |
| Privacy / path history scrub | In progress for residual design-doc paths; tip clean |
| Tag **`v0.2.3`** | Cutting now |
| README install → `v0.2.3` | Packaging |
| `scripts/verify.sh` | Required green on tip before tag |
| Sticky paste fix, S4 `Esc w A`, checklist, pad+dashboard tests | In 0.2.3 body |

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
