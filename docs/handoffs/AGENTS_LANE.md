# AGENTS lane — live handoff

**Owner:** Agents-lane Grok thread  
**Branch / worktree:** `agents/w2-lane` @ `/Users/wcasse/WILL PROJECTS/wscrpt-agents`  
**Base tip when this note written:** `0f2999d` + ACP process wire (local)  
**Updated:** 2026-08-01

Sister lane: **STICKIES** owns the floating notepad. Contract: [../LANES.md](../LANES.md).

## Product truth (Agents)

One bottom **Agents dashboard** is the only agent activity surface:

- `Esc w a` / `:agent` — start goal
  - `agent.use_fake = true` (default) → deterministic fake plan-first loop
  - `use_fake = false` + ready host → ACP process via `agent.argv` (e.g. `grok agent stdio`)
- `Esc w x` — cancel (kills process group when ACP is live)
- `Esc w D` / `:agent-dashboard` (also `:agents`, `:agent-activity`, `:agent-receipt`, `:agent-status`) — toggle dashboard
- **`Esc w A` unbound** — reserve only when a distinct agent action appears
- Auto-open + deeper panel height when run live or receipt non-empty
- Host auth probe: `wscrpt --health`, `docs/AGENT_AUTH.md`, `agent.use_fake` default true

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
| Dirty-tree protection / worktree isolation | Open |
| Full process/PTY restore polish | Partial (process group kill on cancel) |
| Dirty-tree gate | **Done** — hard refuse dirty buffers; soft confirm when Git paths dirty |
| Review packet object (W3) | Open |

## Next work (priority order)

1. **Review handoff** — strengthen “open Git status / diffs” from Review state without inventing a new VCS UI.
2. Keep dashboard the single surface; expand depth/content only.

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

- **2026-08-01:** Richer ACP map: path_touched from tool locations; coalesced message chunks.
- **2026-08-01:** Dirty-tree gate: refuse unsaved buffers; confirm on Git dirt.
- **2026-08-01:** Needs You: ACP permission prompts on dashboard; Y/N + `Esc w A` allow.
- **2026-08-01:** ACP process wire: `src/agent_acp.rs` + `spawn_process_agent`; fake path remains default; dep `serde_json`.
- **2026-08-01:** Lane ownership established; worktree `wscrpt-agents` on `agents/w2-lane`; dashboard consolidation leftovers on `main` (`f7d76fd`).
