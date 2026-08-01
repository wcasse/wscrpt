# AGENTS lane — live handoff

**Owner:** Agents-lane Grok thread  
**Branch / worktree:** `agents/w2-lane` @ `/Users/wcasse/WILL PROJECTS/wscrpt-agents`  
**Tip (pushed):** `8f442b6` (+ review handoff, live-demo checklist; tip may advance)  
**Updated:** 2026-08-01

Sister lane: **STICKIES** owns the floating notepad. Contract: [../LANES.md](../LANES.md).

---

## AFTER COMPACTION — first prompt (do this first)

> **How else could you improve the performance of the agentic integration of this IDE prioritizing performance, simplicity, and benefit to the user?**  
> Context: launch tonight; Agents lane owns bottom dashboard + ACP; Stickies is a separate thread. Form follows function. Worktree: `wscrpt-agents` / `agents/w2-lane`. Do not touch stickies. Prefer small high-leverage perf/UX wins over new features.

**Resume order:** read this file → [AGENTS_LIVE_DEMO.md](AGENTS_LIVE_DEMO.md) → implement top item under **Performance backlog** below (or re-rank with Will).

## Product truth (Agents)

One bottom **Agents dashboard** is the only agent activity surface:

- `Esc w a` / `:agent` — start goal
  - `agent.use_fake = true` (default) → deterministic fake plan-first loop
  - `use_fake = false` + ready host → ACP process via `agent.argv` (e.g. `grok agent stdio`)
- `Esc w x` — cancel (kills process group when ACP is live)
- `Esc w D` / `:agent-dashboard` (also `:agents`, …) — toggle dashboard
- `Esc w A` — **approve** Needs You permission
- `Esc w G` / `:agent-review` — review handoff → Git status (+ single-path diff)
- Dirty-tree: unsaved buffers blocked; Git dirt → Y/Esc confirm
- Live smoke checklist: [AGENTS_LIVE_DEMO.md](AGENTS_LIVE_DEMO.md)
- Host auth probe: `wscrpt --health`, `docs/AGENT_AUTH.md`

No separate Agent Activity popup. Receipt/detail lives in the dashboard via `format_receipt_lines`.

## Shipped (relevant)

| Item | Status |
| --- | --- |
| W0 contracts + fake agent admission | Done |
| W2 fake run loop | Done (partial) |
| Bottom Agents dashboard | Done (single surface) |
| Activity popup removed | Done |
| Host auth readiness (no secrets) | Done |
| Live ACP stdio process wire | **Done (minimal)** — initialize / session/new / session/prompt; plan + tool_call → receipt; cancel kills process group |
| Needs You permission prompts | **Done** — dashboard + Y/N / Esc w A allow; ACP `session/request_permission` |
| Full process/PTY restore polish | Partial (process group kill on cancel) |
| Dirty-tree gate | **Done** — hard refuse dirty buffers; soft confirm when Git paths dirty |
| Richer ACP path/chunk mapping | **Done** |
| Review handoff → Git | **Done** — auto + Esc w G |
| Live demo checklist | [AGENTS_LIVE_DEMO.md](AGENTS_LIVE_DEMO.md) |
| Review packet object (W3) | Open |

## Next work (priority order)

1. **Performance / simplicity backlog** (below) — primary focus until launch.
2. Human live ACP smoke (see [AGENTS_LIVE_DEMO.md](AGENTS_LIVE_DEMO.md)).
3. Optional later: linked worktree isolation (roadmap); W3 review packets.

## Performance backlog (perf → simplicity → user benefit)

Ranked for **tonight’s launch**: ship small wins that keep SSH/iPad responsive, keep one mental model (dashboard + existing Git), and avoid new surfaces.

| Rank | Item | Why (P/S/U) | Effort | Where |
| --- | --- | --- | --- | --- |
| **1** | **Throttle agent UI redraws** while live | P: fewer full paints over mosh; U: less flicker | S | `poll_agent_events` → coalesce admits into one redraw/status per poll budget (reuse background redraw throttle pattern) |
| **2** | **Dashboard view: cheap path** | P: avoid re-`format_receipt_lines` + string thrash every frame; S: cache last N lines invalidated on admit | S–M | `agent_dashboard_view` / paint |
| **3** | **Bound ACP stdout + drop backpressure** | P: stuck agent if channel full; U: predictable cancel | S | `EVENT_CAPACITY`, non-blocking send or drop oldest Notice/chunk flushes; never block kill path |
| **4** | **Smarter review handoff** | P: skip double virtual buffers when idle/clean; U: less noise | S | `handoff_agent_review`: only open status if git.changes > 0 or paths non-empty; single path → prefer **diff only** |
| **5** | **Defer Git status shell-out** | P: review handoff currently runs `git status` sync on UI thread | M | Snapshot already in `GitState` when Ready — paint from cache; optional async refresh |
| **6** | **Status-line rate limit** | P/U: every admit rewrites footer; keep last summary, update footer ≤ ~10 Hz while Working | S | `poll_agent_events` |
| **7** | **Chunk coalesce tuning** | P: fewer receipt events; U: still readable “agent: …” lines | S | `CHUNK_FLUSH_*` constants; maybe status-only for chunks, receipt only on flush-to-plan/tool |
| **8** | **Fake path as default forever for demos** | S/U: zero risk in CI/iPad demos; process mode opt-in (already) | — | Keep; document in live demo only |
| **Defer** | Full ACP SDK / FS-terminal client | Complex; little launch ROI | L | Not tonight |
| **Defer** | Worktree isolation | Benefit high, risk high pre-launch | L | Roadmap |

### Recommendation (pick for next code turn)

**#1 + #4 done** on this branch (batch status + background paint throttle; quieter handoff).  
Next optional: **#2** dashboard line cache, **#3** ACP channel backpressure.

## Do not

- Edit `src/stickies.rs` or sticky pad paint/key paths.
- Reintroduce Agent Activity overlay.
- Mix Stickies UX into agent commits.
- Force-push `main`.

## Shared-file etiquette for this lane

When editing `app.rs` / `render.rs` / `keymap.rs` / `command.rs` / `session.rs`:

- Touch only agent-named symbols and agent keymap entries.
- Leave `sticky_pad_*` and Stickies actions untouched.
- After Stickies merges, rebase `agents/w2-lane` on `main` before continuing.

## Verify command (this lane)

```sh
cd "/Users/wcasse/WILL PROJECTS/wscrpt-agents"
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

## Log

- **2026-08-01:** Implemented #1+#4: agent batch status + background redraw throttle; quieter review handoff.
- **2026-08-01:** Performance backlog + compaction first-prompt planted (throttle redraw, cheap dashboard, quieter handoff).
- **2026-08-01:** Live demo checklist: [AGENTS_LIVE_DEMO.md](AGENTS_LIVE_DEMO.md) (host has grok + auth; config still fake by default).
- **2026-08-01:** Review handoff: auto-open Git status (+ single path diff); Esc w G / :agent-review.
- **2026-08-01:** Richer ACP map: path_touched from tool locations; coalesced message chunks.
- **2026-08-01:** Dirty-tree gate: refuse unsaved buffers; confirm on Git dirt.
- **2026-08-01:** Needs You: ACP permission prompts on dashboard; Y/N + `Esc w A` allow.
- **2026-08-01:** ACP process wire: `src/agent_acp.rs` + `spawn_process_agent`; fake path remains default; dep `serde_json`.
- **2026-08-01:** Lane ownership established; worktree `wscrpt-agents` on `agents/w2-lane`; dashboard consolidation leftovers on `main` (`f7d76fd`).
