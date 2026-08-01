# wscrpt dual-thread status

**Date:** 2026-08-01  
**Goal:** open-source ready today  
**Local tip:** (see `git rev-parse --short HEAD`; was 15+ ahead of `origin/main` @ `905d010`)  
**Do not push without Will.** Do not rewrite history without Will.

This file is the coordination board for concurrent agent threads. Claim a
lane before editing its paths. Prefer append-only updates under “Live log”.

## Thread lanes (claim one)

| Lane | Owner | Paths / surface | Goal |
| --- | --- | --- | --- |
| **A — Launch / publish** | *this thread (Grok open-source)* | `docs/OPEN_*`, `docs/PUBLIC_*`, `docs/STATUS.md`, `docs/releases/`, `docs/PUBLISH_RUNBOOK.md`, packaging | Green gates, release artifacts, crates.io dry-run, push/tag plan |
| **B — Product polish** | *other thread preferred* | `src/**` (app/keymap/render/git/agent), UX, job-list product items | Finish any remaining MUST DO product work without packaging thrash |
| **C — Native / preview** | *claim if needed* | `clients/`, `previewd/`, `docs/NATIVE_*`, `docs/REMOTE_*` | Keep out of Rust crate; real-device is human-gated |

Rule: if you touch `src/`, leave Lane A alone on version bumps / release notes
until your commit is in. If you touch release docs, do not refactor `src/`.

## Job list reconciliation

### MUST DO

| Item | Status | Evidence |
| --- | --- | --- |
| Integrate viewport / native terminal+player workspace | **Done (code)** | `4c8f3a9`; human device pass still open |
| Pre-publish codebase audit | **Snapshot green** | `scripts/audit-public-source.sh` |
| `wscrpt` on PATH anywhere on Groudon | **Done** | `cargo install --path . --locked --force`; works from `/tmp` |
| Host not Mac-only (Linux/WSL) | **Done (docs + CI contract)** | `docs/HOST_SUPPORT.md` |
| Swap `Esc l` / `Esc L` | **Done** | `Esc l` select lines; `Esc L` line numbers |

### THINGS TO UPGRADE

| Item | Status | Evidence |
| --- | --- | --- |
| First-edit transition cue | **Done** | CHANGELOG 0.2.1 |
| Git integration | **Done (trusted local slice)** | stage/unstage/commit |
| Magic Keyboard / missing-keyboard warnings | **Native client done; terminal path partial** | PreviewHarness banner |

### WISHLIST

| Item | Status | Notes |
| --- | --- | --- |
| Agent-native | **W0 in 0.2.1** | Contracts only; no ACP UI |
| Stickies | **Done v1** | `7a650a9` |
| Collaboration | **Roadmap only** | No CRDT v1 |

## Open-source checklist (live)

| Gate | Status | Blocker |
| --- | --- | --- |
| Public repo | Yes | https://github.com/wcasse/wscrpt |
| MIT + community docs | Yes | LICENSE, CONTRIBUTING, SECURITY, CoC |
| Snapshot privacy audit | **Pass** | Re-run on exact publish commit |
| History identity | **Recommend intentional public** | No rewrite |
| Full local verify | **Pass** | `scripts/verify.sh` |
| `cargo publish --dry-run` | **Pass** | crates.io not published |
| Strategy A release fold | **Done** | Unreleased → `[0.2.1]`; notes + runbook |
| `gh` auth | **BROKEN** | `gh auth login -h github.com` |
| Push local main | **Pending human** | ~15 commits ahead |
| Tag `v0.2.1` | **Missing** | Only `v0.2.0` on origin |
| GitHub Release pages | **Missing** | Runbook ready |
| Topics + private vuln reporting | **Blocked on gh** | Runbook ready |

## Chosen publish path: Strategy A (fast)

Fold Unreleased into **0.2.1**, tag **HEAD** as `v0.2.1`, one ship.

Exact commands: [`docs/PUBLISH_RUNBOOK.md`](PUBLISH_RUNBOOK.md).

## Live log

- **2026-08-01 Lane A:** Audit + verify + dry-run green. Reinstalled PATH binary. Other thread landed Stickies `7a650a9`.
- **2026-08-01 resume Lane A:** Strategy A fold into CHANGELOG 0.2.1 + expanded `docs/releases/v0.2.1.md` + `docs/PUBLISH_RUNBOOK.md`. **Still blocked on `gh` auth and human push.**
- **Other thread:** claim Lane B/C here with tip SHA when active.
