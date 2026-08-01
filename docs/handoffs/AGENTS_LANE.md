# AGENTS lane — live handoff

**Owner:** Agents-lane Grok thread  
**Branch / worktree:** `agents/w2-lane` @ sibling worktree `wscrpt-agents`  
**Base tip when this note written:** `0f2999d` on `main`  
**Updated:** 2026-08-01 (SHIP freeze notice)

Sister lane: **STICKIES** owns the floating notepad. **SHIP** owns release packaging only. Contract: [../LANES.md](../LANES.md).

## SHIP freeze (v0.2.2 tonight)

- Land or park feature work; after freeze, only SHIP docs/version commits land on `main`.
- Rebase this branch on tagged `v0.2.2` / updated `main` before the next Agents commit.
- SHIP will **not** edit `src/agent*.rs` or dashboard paint.

## Product truth (Agents)

One bottom **Agents dashboard** is the only agent activity surface:

- `Esc w a` / `:agent` — start goal (fake plan-first loop until ACP lands)
- `Esc w x` — cancel
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
| Live ACP `grok agent stdio` | **Not started** |
| Needs You permission prompts | Open |
| Dirty-tree protection / worktree isolation | Open |
| Real process group + PTY restore | Open |
| Review packet object (W3) | Open |

## Next work (priority order)

1. **ACP process wire** — spawn `Config.agent.argv` when `use_fake = false` and readiness passes; keep fake path as default.
2. **Needs You** — map approval-shaped agent events to dashboard emphasis + status; no second modal if the dashboard can host it.
3. **Dirty-tree gate** — refuse or confirm before run when workspace dirty (roadmap gate).
4. **Review handoff** — strengthen “open Git status / diffs” from Review state without inventing a new VCS UI.
5. Keep dashboard the single surface; expand depth/content only.

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
cd /path/to/wscrpt-agents
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

## Log

- **2026-08-01 (SHIP):** Freeze window for **v0.2.2** — land ACP wire or park on this branch; no absolute home paths in tracked docs.
- **2026-08-01:** Lane ownership established; worktree `wscrpt-agents` on `agents/w2-lane`; dashboard consolidation leftovers on `main`.
