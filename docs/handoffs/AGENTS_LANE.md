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

- `Esc w a` / `:agent` — start goal (fake plan-first loop until Pi/ACP process lands)
- `Esc w x` — cancel
- `Esc w D` / `:agent-dashboard` (also `:agents`, `:agent-activity`, `:agent-receipt`, `:agent-status`) — toggle dashboard
- **`Esc w A` — approve Needs You** (lane baseline; **not** sticky write-back). Today on `main` it still maps to sticky receipt apply until the Needs You PR rebinds it and regenerates `docs/COMMANDS.md`.
- `:apply-receipt` — sticky receipt → `## Log` after REVIEW (S4); will keep colon when `A` becomes approve
- `Esc w C` / `Esc w Y` — checklist fan-out / apply checkmarks (S2)
- Auto-open + deeper panel height when run live or receipt non-empty
- Host auth probe: `wscrpt --health`, `docs/AGENT_AUTH.md`, `agent.use_fake` default true
- **Pi:** `profile = "pi"`, gate at `pi/extensions/wscrpt-permission-gate.ts` (tool confirm → RPC UI request)

No separate Agent Activity popup. Receipt/detail lives in the dashboard via `format_receipt_lines`.

**Keymap rule:** any `src/keymap.rs::COMMANDS` change →  
`cargo run --locked -- --print-command-reference > docs/COMMANDS.md`

## Shipped (relevant)

| Item | Status |
| --- | --- |
| W0 contracts + fake agent admission | Done |
| W2 fake run loop | Done (partial) |
| Bottom Agents dashboard | Done (single surface) |
| Activity popup removed | Done |
| Host auth readiness (no secrets) | Done |
| Sticky brief attach (S1) | Done |
| Checklist fan-out + human apply (S2) | Done |
| Receipt → sticky `## Log` (S4) | Done |
| Live ACP `grok agent stdio` | **Done (0.2.4)** — NDJSON client; permissions auto-cancelled |
| Needs You permission prompts | Open |
| Dirty-tree protection / worktree isolation | Open |
| Real process group + PTY restore | Open |
| Review packet object (W3) | Open |

## Next work (priority order)

1. ~~**ACP process wire**~~ — done in 0.2.4 for generic ACP argv.
2. **Pi RPC client** (`agent_pi`) — spawn with gate `--extension`; map events; cancel.
3. **Needs You + keymap** — `Esc w A` = approve; deny action; rebind sticky apply; regenerate COMMANDS.md; answer `extension_ui_response`.
4. **Model picker** — `Esc w m` via Pi `get_available_models` / `set_model`.
5. **Dirty-tree gate** — refuse or confirm before run when workspace dirty.
6. **Review handoff** — strengthen Git status/diff from Review without a new VCS UI.
7. Keep dashboard the single surface; expand depth/content only.

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

- **2026-08-03:** Phase 1 Pi plumbing — permission gate TS + `profile=pi` health notes + argv/`--extension` helpers; plan baseline `Esc w A` = approve.
- **2026-08-01 (SHIP):** Freeze window for **v0.2.2** — land ACP wire or park on this branch; no absolute home paths in tracked docs.
- **2026-08-01:** Lane ownership established; worktree `wscrpt-agents` on `agents/w2-lane`; dashboard consolidation leftovers on `main`.
