# Contributor map

Where to edit without reading all of `src/app.rs` first. Pair with [ARCHITECTURE.md](ARCHITECTURE.md) for runtime boundaries and [CONTRIBUTING.md](../CONTRIBUTING.md) for PR expectations.

## Runtime flow

`src/main.rs` owns CLI flags, terminal entry/restore, event polling, redraw cadence, full-screen shell handoff, and session bootstrap. It creates the workspace and `App`, enters TUI mode, then starts background services. `App` in `src/app.rs` is the runtime façade: it owns grouped UI / project / LSP / task / persistence / Git state, routes keys through `keymap` → `execute_action` / prompts / colon commands, admits background work only via `poll_services`, and feeds `render` with read-only views. Domain modules (`document`, `editor`, `workspace`, `search`, `tasks`, `git`, `lsp*`, `session`, `recovery`, `services`, …) hold the actual algorithms and I/O; prefer changing those modules and thin wiring in `App` rather than growing new policy inside the façade.

## Large `app.rs` and mutation boundaries

`src/app.rs` is intentionally large (~18k lines): prompts, action dispatch, service admission, and feature orchestration live there. **Do not treat it as a free place to dump new subsystems.** Prefer a focused module plus a small call site from `App`. Mutations that touch the filesystem or external tools must stay inside the 0.2 boundary:

- **In-editor:** buffer edits, atomic save / Save As / Save Copy As, project create/rename, single-document LSP format, and the trust-gated local Git stage-current / unstage-current / commit-staged slice.
- **Out of bounds:** project-wide replace, LSP rename/code actions/workspace edits, Git branch/network/discard/reset/clean/arbitrary-path operations, and embedded terminals. Use a trusted task or `Esc t t` / `:terminal`, then refresh/reload.

See [ARCHITECTURE.md](ARCHITECTURE.md) (mutation boundaries) and [CHANGELOG.md](../CHANGELOG.md) (0.2 removed commands and non-goals). Do not reintroduce removed surface area without an explicit design discussion.

## Where do I change X?

| Concern | Primary files | Notes |
| --- | --- | --- |
| **Keybindings / command palette / action IDs** | `src/keymap.rs` (`COMMANDS`) | Authoritative registry: bindings, titles, namespaces, docs. After changes, regenerate `docs/COMMANDS.md`. |
| **Colon (`:`) commands** | `src/command.rs` → dispatch in `src/app.rs` | Parser and `ExCommand` variants; execution stays in `App`. |
| **Save / atomic write / external-change detect** | `src/document.rs`; orchestration in `src/app.rs` | `Document::save`, `save_as`, `save_copy_as`, hard-link refusal. App owns prompts and multi-buffer Save All. |
| **Buffer editing / cursor / undo** | `src/editor.rs`, `src/document.rs`, `src/text.rs`, `src/wrap.rs`, `src/visual.rs` | Rope and edit model live under document/editor; App routes insert/delete keys. |
| **Multi-buffer workspace** | `src/workspace.rs` | Open buffers, active index, root discovery markers. |
| **In-buffer find/replace** | `src/app.rs` (prompts + match state); buffer text via editor/document | Project-wide **search** is separate (below). No multi-file replace. |
| **Project search / Quick Open index** | `src/search.rs`, `src/project.rs`; worker + prompts in `src/app.rs` | Index/tree snapshots refresh via services; search is generation-cancellable. |
| **Tasks** | `src/tasks.rs`, `src/task_output.rs`, `src/task_problem.rs`; trust UI in `src/app.rs` | `.wscrpt/tasks.toml` argv only; trust gate before every run. |
| **Git** | `src/git.rs`; snapshot/mutation workers in `src/services.rs`; `GitState` + trust UI in `src/app.rs` | Inspection is read-only. Only saved-current stage/unstage and commit-staged are admitted; one at a time, trust every run, then refresh status. |
| **LSP protocol / client / UI adapters** | `src/lsp.rs`, `src/lsp_client.rs`, `src/lsp_session.rs`, `src/lsp_ui.rs`, `src/lsp_discover.rs` | Config: `src/config.rs` + user `config.toml`. Orchestration/sync in `App` `LspState`. Format = single-document only. |
| **Render / layout / redraw** | `src/render.rs`; layout accessors on `App` | Differential TUI; no business logic in the renderer. |
| **Session restore** | `src/session.rs`; bootstrap in `src/main.rs` / `App` | Session v2: paths + layout metadata, never buffer text. `--no-session` in CLI. |
| **Crash recovery journals** | `src/recovery.rs`; listing via `services` + `App` | Unsaved text only; atomic journal writes. |
| **Background services** | `src/services.rs`; admission `App::poll_services` | Project index, Git snapshot/mutation, recovery scans. Stale results dropped by workspace id + generation; Git mutation is distinct and single-flight. |
| **Lane ownership (Agents vs Stickies)** | [LANES.md](LANES.md), [handoffs/AGENTS_LANE.md](handoffs/AGENTS_LANE.md), [handoffs/STICKIES_LANE.md](handoffs/STICKIES_LANE.md) | Dual concurrent threads: exclusive modules + shared-file edit protocol. Agents worktree: `../wscrpt-agents` (`agents/w2-lane`). |
| **Agent-native contracts (W0)** | `src/agent_contract.rs`, `src/agent.rs` | Work packets, events, review/Sticky *types*; `AgentCoordinator` + `FakeAgent`. **AGENTS lane.** |
| **Stickies (pad UI)** | `src/stickies.rs` (`StickyPad`); paint in `src/render.rs`; keys in `App` | Floating top-right card; Markdown storage; `Esc w k` / `K`. **STICKIES lane** — do not edit from Agents lane. |
| **Agent run (W2 partial)** | `src/agent_runtime.rs`, `src/agent_acp.rs`, `src/agent.rs`; keys in `keymap` / `App` | Fake plan-first loop by default; ACP process when `use_fake = false`. `Esc w a` / `x` / `D`. **AGENTS lane.** |
| **Agents dashboard UI** | `src/render.rs` (`agent_dashboard_height`, `paint_agent_dashboard`); `App::agent_dashboard_view` | Single bottom strip: roster + full receipt (no activity popup). **AGENTS lane.** |
| **Agent auth readiness** | `src/agent_auth.rs`; `Config.agent`; `wscrpt --health`; `docs/AGENT_AUTH.md` | Host CLI owns secrets; wscrpt only probes PATH / env names / markers. **AGENTS lane.** |
| **CLI flags / startup / shell handoff** | `src/main.rs` | `Cli` (clap): path, `--project`, `--mouse` / `--no-mouse`, `--no-osc52`, `--print-default-config`, `--print-command-reference`, `--health`, `--input-diagnostics`, `--no-session`. |
| **Config defaults / language servers** | `src/config.rs`; default text in `src/main.rs`; discovery `src/lsp_discover.rs` | LSP only from user-global config, never workspace files. `format_on_save` lives on `Config`. |
| **First-run help** | `src/onboarding.rs`; open from `src/main.rs` via `App::maybe_open_first_run_help` | XDG state flag; set `WSCRPT_SKIP_FIRST_RUN_HELP=1` in automation. |
| **Terminal modes / panic restore** | `src/terminal.rs` | Alternate screen, raw mode, bracketed paste, mouse capture. |
| **Clipboard / OSC 52** | `src/clipboard.rs` | Internal register always; OSC 52 optional and bounded. |
| **Syntax highlight** | `src/syntax.rs` | Fed into render; keep limits explicit. |

## Regenerate `docs/COMMANDS.md`

After any change to `src/keymap.rs::COMMANDS`:

```sh
cargo run --locked -- --print-command-reference > docs/COMMANDS.md
```

A unit test fails if the committed file drifts from the registry.

## Verify and MSRV

**MSRV:** Rust **1.88** (`rust-version` in [Cargo.toml](../Cargo.toml)).

Host gate (fmt, clippy `-D warnings`, tests, package, isolated install):

```sh
scripts/verify.sh
```

Typical local loop before a PR:

```sh
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

On Linux CI, `WSCRPT_REQUIRE_TMUX=1` makes a missing tmux a hard failure. See also [RELEASING.md](RELEASING.md) and [IPAD_BLINK_QA.md](IPAD_BLINK_QA.md) for release and human gates.
