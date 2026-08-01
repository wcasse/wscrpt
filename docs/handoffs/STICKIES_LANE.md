# STICKIES lane — live handoff

**Owner:** Stickies-lane Grok thread  
**Branch:** recommend `stickies/*` (not `agents/w2-lane`)  
**Base tip when this note written:** `f7d76fd` on `main`  
**Updated:** 2026-08-01 (scaffold for Stickies lane — please replace with your live status)

Sister lane: **AGENTS** owns the bottom Agents dashboard and run loop. Contract: [../LANES.md](../LANES.md).

## Product truth (Stickies) — from last known ship

- Floating **top-right notepad** (Mac Stickies–style), not “open another buffer” by default.
- Keys: `Esc w k` toggle pad · `Esc w K` new personal note.
- Storage: Markdown + TOML front matter; personal XDG + team `.wscrpt/stickies/`.
- Geometry/visibility session-local (`sticky_pad_visible`); do not commit pad geometry into the repo.

## AGENTS lane will not

- Edit `src/stickies.rs`
- Change sticky pad paint or sticky key chords
- Re-open stickies as the default buffer UX

## Please do not (for Agents success)

- Edit `src/agent*.rs` or agent dashboard paint/height logic
- Bind `Esc w A` without coordinating (Agents left it free)
- Mix agent run/dashboard work into Stickies commits
- Force-push `main`

## Shared files

See [../LANES.md](../LANES.md). Touch only sticky-named regions in `app.rs` / `render.rs` / `keymap.rs` / `command.rs` / `session.rs`.

## Log

- **2026-08-01:** Scaffold created by Agents lane so ownership is visible. Stickies lane: overwrite this file with your real next steps and branch name.
