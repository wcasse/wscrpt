# wscrpt dual-thread status

**Date:** 2026-08-01  
**Goal:** open-source ready today  
**Local tip:** `efc0931` (14 commits ahead of `origin/main` @ `905d010`)  
**Do not push without Will.** Do not rewrite history without Will.

This file is the coordination board for concurrent agent threads. Claim a
lane before editing its paths. Prefer append-only updates under “Live log”.

## Thread lanes (claim one)

| Lane | Owner | Paths / surface | Goal |
| --- | --- | --- | --- |
| **A — Launch / publish** | *this thread (Grok open-source)* | `docs/OPEN_*`, `docs/PUBLIC_*`, `docs/STATUS.md`, `docs/releases/`, `scripts/audit*`, `scripts/verify.sh`, `.gitignore`, packaging | Green gates, release artifacts, crates.io dry-run, push/tag plan |
| **B — Product polish** | *other thread preferred* | `src/**` (app/keymap/render/git/agent), UX, job-list product items | Finish any remaining MUST DO product work without packaging thrash |
| **C — Native / preview** | *claim if needed* | `clients/`, `previewd/`, `docs/NATIVE_*`, `docs/REMOTE_*` | Keep out of Rust crate; real-device is human-gated |

Rule: if you touch `src/`, leave Lane A alone on version bumps / release notes
until your commit is in. If you touch release docs, do not refactor `src/`.

## Job list reconciliation

### MUST DO

| Item | Status | Evidence |
| --- | --- | --- |
| Integrate viewport / native terminal+player workspace | **Done (code)** | `4c8f3a9`; human device pass still open |
| Pre-publish codebase audit | **Snapshot green** | `scripts/audit-public-source.sh` passed 2026-08-01; history intentionally red until owner decision |
| `wscrpt` on PATH anywhere on Groudon | **Done** | `cargo install --path . --locked --force`; works from `/tmp` |
| Host not Mac-only (Linux/WSL) | **Done (docs + CI contract)** | `docs/HOST_SUPPORT.md`; native Windows deferred |
| Swap `Esc l` / `Esc L` | **Done** | `Esc l` = select lines; `Esc L` = line numbers (`src/keymap.rs`) |

### THINGS TO UPGRADE

| Item | Status | Evidence |
| --- | --- | --- |
| First-edit transition cue | **Done** | `5134423`; Unreleased CHANGELOG |
| Git integration | **Done (trusted local slice)** | `5aaa415` stage/unstage/commit; network/branch still shell |
| Magic Keyboard / missing-keyboard warnings | **Native client done; terminal path partial** | `PhysicalKeyboardMonitor` + banner in PreviewHarness; Blink/SSH path is host TUI only |

### WISHLIST

| Item | Status | Notes |
| --- | --- | --- |
| Agent-native | **W0 done** | Contracts + `AgentCoordinator` + fake agent; W1+ not shipped |
| Stickies | **Done v1 (`7a650a9`)** | Module + TUI list/create/archive (`Esc w k` / `Esc w K`); layout UI still deferred |
| Collaboration | **Roadmap only** | Same doc; no CRDT v1 |

## Open-source checklist (live)

| Gate | Status | Blocker |
| --- | --- | --- |
| Public repo | Yes | https://github.com/wcasse/wscrpt |
| MIT + community docs | Yes | LICENSE, CONTRIBUTING, SECURITY, CoC |
| Snapshot privacy audit | **Pass** | Re-run on exact publish commit |
| History identity | **Owner decision** | Reachable history has `163216174+wcasse@users.noreply.github.com`; rewrite **not** authorized |
| `gh` auth | **BROKEN** | `gh auth status` → invalid token; blocks Releases API, topics, private vuln UI via CLI |
| Push 14 local commits | **Pending human** | Includes 0.2.1 prep + native client + Unreleased product + Stickies |
| Tag `v0.2.1` | **Missing** | Only `v0.2.0` exists on origin; README already points at `v0.2.1` |
| GitHub Release pages | **Missing** | Drafts in `docs/releases/` |
| crates.io | **Dry-run pass; not published** | `cargo publish --dry-run` green on tip |
| Topics | **Unknown / blocked** | Need working `gh` |
| Private vulnerability reporting | **Unknown / blocked** | Need repo settings or working `gh` |
| Demo GIF | P1 optional | `docs/demo.tape` present |

## Recommended publish sequence (after verify green)

1. Will: `gh auth login` (or refresh token).
2. Confirm identity decision: **intentional public author** (default) — do **not** rewrite history.
3. Choose tag strategy (pick one):
   - **A (fast):** keep version `0.2.1`, fold Unreleased highlights into `docs/releases/v0.2.1.md`, tag **HEAD** as `v0.2.1`, push branch + tag.
   - **B (cleaner):** tag `v0.2.1` at `dde641d` (prep commit), bump tip to `0.2.2` for native/git/agent/edit-cue work, two tags.
4. `git push origin main` (Will only).
5. `git push origin v0.2.1` (+ `v0.2.2` if B).
6. Create GitHub Releases from `docs/releases/*.md` (checksums if attaching binaries).
7. `cargo publish` after dry-run green.
8. Repo settings: topics + private vulnerability reporting.
9. Reinstall on Groudon from the published tag.

## Live log

- **2026-08-01 this thread (Lane A):** Snapshot audit **pass**. `scripts/verify.sh` **pass** (fmt/clippy/629 lib+bin tests/tmux/pty/docs/package/isolated install). `cargo publish --dry-run` **pass** (76 files, 2.2MiB). Reinstalled `~/.cargo/bin/wscrpt` from tip; works from `/tmp`. `gh` token invalid. No push.
- **2026-08-01 other thread (Lane B):** Stickies v1 committed as `7a650a9`.
- **Next human gates:** `gh auth login` → choose tag strategy → `git push origin main` + tags → GitHub Releases from `docs/releases/` → `cargo publish` → topics + private vuln reporting.
