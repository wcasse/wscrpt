# Dual-lane ownership — Agents vs Stickies

**Date:** 2026-08-01  
**Rule:** form follows function; one lane owns each product surface.

Two concurrent Grok threads (or humans) may work in this repo at once:

| Lane | Owner (current) | Product surface |
| --- | --- | --- |
| **AGENTS** | Agents-lane thread | Bottom Agents dashboard, run loop, auth, contracts, ACP |
| **STICKIES** | Stickies-lane thread | Floating top-right notepad, sticky storage, pad UX |

This file is the contract between lanes. Prefer reading it before editing shared files.

## Exclusive ownership (do not cross without a handoff)

### AGENTS owns

| Path | Role |
| --- | --- |
| `src/agent.rs` | Coordinator, admission, fake agent |
| `src/agent_contract.rs` | Packets, events, review types (shared *types* only) |
| `src/agent_runtime.rs` | Run loop, spawn, receipt formatting |
| `src/agent_auth.rs` | Host readiness probes (no secrets) |
| `docs/AGENT_AUTH.md` | Auth setup |
| `docs/AGENT_NATIVE_ROADMAP.md` | Agent phases (update W2+; leave W1 Stickies notes to Stickies lane) |
| `docs/handoffs/AGENTS_LANE.md` | Live Agents status / next steps |

Keys / commands (Agents): `Esc w a`, `Esc w x`, `Esc w D`; `:agent*`, `:agents`, dashboard aliases.

### STICKIES owns

| Path | Role |
| --- | --- |
| `src/stickies.rs` | Library, Markdown I/O, `StickyPad` |
| Sticky sections of `docs/AGENT_NATIVE_ROADMAP.md` (W1 / W6 notes) | Product truth for notes |
| `docs/handoffs/STICKIES_LANE.md` | Live Stickies status (Stickies lane creates/maintains) |

Keys / commands (Stickies): `Esc w k`, `Esc w K`; sticky pad focus/edit chords; picker archive/delete if still used.

## Shared files — edit protocol

These are **hot** and caused cross-lane damage before. Touch only your named region; never “while I’m here” refactors.

| File | AGENTS may touch | STICKIES may touch |
| --- | --- | --- |
| `src/app.rs` | `AgentUiState`, agent run/dashboard methods, `Action::Agent*`, agent status strings, agent prompt flows | `sticky_pad` field, sticky pad key handling, sticky library helpers, stickies prompts |
| `src/render.rs` | `agent_dashboard_*`, `paint_agent_dashboard`, agent height constants | `paint_sticky_pad`, sticky card chrome only |
| `src/keymap.rs` | `AgentRun` / `AgentCancel` / `AgentDashboard` only | `Stickies` / `NewSticky` only |
| `src/command.rs` | `Agent*` ex-commands and aliases | Stickies ex-commands and aliases |
| `src/session.rs` | `agent_dashboard_visible` only | `sticky_pad_visible` only |
| `src/config.rs` / `src/main.rs` | `agent` config, `--health` agent probe | sticky-related config only if introduced |
| `docs/COMMANDS.md` | Regenerate after *your* keymap change; include the other’s keys as already registered | Same |
| `CHANGELOG.md` / `README.md` | Agent bullets only | Stickies bullets only |
| `docs/CONTRIBUTOR_MAP.md` | Agent rows | Stickies rows |

**Hard rules**

1. **Do not** rewrite the other lane’s exclusive modules.
2. **Do not** reintroduce an Agent Activity popup; Agents surface is the bottom dashboard only.
3. **Do not** open stickies as ordinary buffers as the default UX; pad owns jotting.
4. Before editing a shared file, `git pull --ff-only` (or rebase your lane branch on latest `main`).
5. Prefer **lane branches** over both editing `main` live:
   - Agents: `agents/w2-lane` (worktree: `../wscrpt-agents`)
   - Stickies: `stickies/*` (recommend a dedicated worktree if both code at once)
6. If you must touch a shared region the other owns (e.g. layout interaction between pad and dashboard), write a one-line note in *both* handoff files and keep the diff minimal.

## Worktrees (recommended)

```text
/Users/wcasse/WILL PROJECTS/wscrpt          → main or Stickies branch
/Users/wcasse/WILL PROJECTS/wscrpt-agents   → agents/w2-lane   (AGENTS lane)
```

Agents lane should do feature work only in `wscrpt-agents`. Stickies lane stays out of that tree.

## Merge / PR order

- Land lane PRs into `main` separately; no mixed “stickies + agents” feature commits.
- After the other lane merges, rebase/ff your lane branch before continuing.
- Do not force-push `main`. Do not amend published commits.

## Verify

Each lane runs before claiming done:

```sh
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

After keymap changes:

```sh
cargo run --locked -- --print-command-reference > docs/COMMANDS.md
```

## Status pointers

- Agents live log: [handoffs/AGENTS_LANE.md](handoffs/AGENTS_LANE.md)
- Stickies live log: [handoffs/STICKIES_LANE.md](handoffs/STICKIES_LANE.md) (Stickies lane)
- Product roadmap: [AGENT_NATIVE_ROADMAP.md](AGENT_NATIVE_ROADMAP.md)
- File map: [CONTRIBUTOR_MAP.md](CONTRIBUTOR_MAP.md)
