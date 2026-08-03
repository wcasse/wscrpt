# STICKIES lane — live handoff

**Owner:** Stickies-lane Grok thread  
**Branch:** recommend `stickies/*` (not `agents/w2-lane`)  
**Base tip when this note written:** `0f2999d` on `main`  
**Updated:** 2026-08-01 (SHIP freeze notice — Stickies lane: replace next-steps with live status)

Sister lanes: **AGENTS** owns the bottom Agents dashboard and run loop. **SHIP** owns release packaging only. Contract: [../LANES.md](../LANES.md).

## SHIP freeze (v0.2.2 tonight)

- Land or park pad polish; after freeze, only SHIP docs/version commits land on `main`.
- Rebase your branch on tagged `v0.2.2` / updated `main` before the next Stickies commit.
- SHIP will **not** edit `src/stickies.rs` or pad paint/keys.

## Product truth (Stickies) — from last known ship

- Floating **top-right notepad** (Mac Stickies–style), not “open another buffer” by default.
- Keys: `Esc w k` pure show/hide toggle · `Esc w K` new personal note.
- Focused pad: type freely (including `[` / `]`); **Ctrl-P / Ctrl-N** cycle notes (tab strip); **Ctrl-S** save · **Ctrl-A** archive · **Ctrl-X** delete; **Esc** unfocus (pad stays glanceable); **Esc w k** hides from focused or glanceable; **Ctrl-K** / Alt-chords enter Action like the editor.
- Multi-note chrome: title row is a compact tab strip (`[n:title*]` active, `‹ ›` overflow).
- Storage: Markdown + TOML front matter; personal XDG + team `.wscrpt/stickies/`.
- Geometry/visibility session-local (`sticky_pad_visible`); do not commit pad geometry into the repo.

## AGENTS / SHIP will not

- Edit `src/stickies.rs`
- Change sticky pad paint or sticky key chords
- Re-open stickies as the default buffer UX

## Please do not (for Agents / SHIP success)

- Edit `src/agent*.rs` or agent dashboard paint/height logic
- Bind `Esc w A` without coordinating (Agents left it free)
- Mix agent run/dashboard work into Stickies commits
- Force-push `main`
- Put absolute developer home paths in tracked docs (public-source audit fails)

## Shared files

See [../LANES.md](../LANES.md). Touch only sticky-named regions in `app.rs` / `render.rs` / `keymap.rs` / `command.rs` / `session.rs`.

## Log

- **2026-08-02:** pure `Esc w k` hide (glanceable no longer re-focuses first); multi-note tab strip on title row + roster titles for flip-through.
- **2026-08-01:** sticky-pad-review follow-ups: bracket typing, Action cancel path, delete dirty race, session restore note load, unpaintable unfocus, paint body-rows (on main with 0.2.3 tip).
- **2026-08-01 (SHIP):** Freeze window for **v0.2.2**.
- **2026-08-01:** Scaffold created by Agents lane so ownership is visible. Stickies lane: overwrite this file with your real next steps and branch name.
