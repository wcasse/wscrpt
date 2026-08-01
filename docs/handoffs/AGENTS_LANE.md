# AGENTS lane — live handoff

**Owner:** Agents-lane Grok thread  
**Branch / worktree:** `agents/w2-lane` @ `/Users/wcasse/WILL PROJECTS/wscrpt-agents`  
**Base tip when this note written:** `f7d76fd` on `main`  
**Updated:** 2026-08-01

Sister lane: **STICKIES** owns the floating notepad. Contract: [../LANES.md](../LANES.md).

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
cd "/Users/wcasse/WILL PROJECTS/wscrpt-agents"
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

## Log

- **2026-08-01:** Lane ownership established; worktree `wscrpt-agents` on `agents/w2-lane`; dashboard consolidation leftovers on `main` (`f7d76fd`).
