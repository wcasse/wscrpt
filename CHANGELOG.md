# Changelog

## [Unreleased]

## [0.2.3] - 2026-08-01

### Added

- **Receipt → sticky log write-back (S4):** sticky-attached agent runs tag receipt events with `artifact_ref = sticky:<id>`; after REVIEW, `Esc w A` / `:apply-receipt` appends a bounded `## Log` block from the receipt (human confirm only — never silent). Coexists with checklist apply (`Esc w Y`).
- Ship-surface regression tests for sticky pad + Agents dashboard coexistence, fake agent review, checklist write-back, and sticky-focused paste.
- Public launch kit assets and demo GIF under `docs/assets/` (README pin matches this tag).

### Fixed

- **Sticky pad paste:** while the pad is focused, paste inserts into the note body instead of the active document.
- Incomplete `Esc w A` wiring that could leave the tree non-compiling during concurrent lane work.
- **Sticky pad key routing (review follow-ups):** bare `[` / `]` type checklist markers; cycle notes with **Ctrl-P / Ctrl-N**. Action layer (Ctrl-K / Alt-chords) mirrors the editor — unknown keys cancel Action instead of editing the pad. Esc unfocus reports save failures. Delete only clears dirty after a successful library delete. Session restore of a visible pad loads a note. Narrow terminals unfocus an unpaintable pad. Card paint body-row math matches chrome (height − 4).

### Changed

- `Esc w A` is bound to **Apply Receipt Log to Sticky** (was unbound after the Agent Activity popup removal).
- Install documentation and honesty pin move from `v0.2.2` to **`v0.2.3`**.

## [0.2.2] - 2026-08-01

### Added

- Agent run loop (W2 partial): plan-first fake agent with work-packet scope, cancel, and review handoff messaging. `Esc w a` / `:agent` starts a goal, `Esc w x` cancels. Default `agent.use_fake = true`; ACP argv is config-ready for host CLIs (for example Grok Build `grok agent stdio`) but the safe default remains fake until you opt in.
- Toggleable bottom **Agents dashboard** strip (Grok Build–inspired): state icons, live/idle roster, session/goal/authority, full receipt (kind + path), and key hints. `Esc w D` / `:agent-dashboard` (also `:agents`, `:agent-activity`, `:agent-receipt`, `:agent-status`) toggles; session-persisted; auto-opens and deepens when a run starts or a receipt exists.
- Stickies are a **floating top-right notepad** (toggle `Esc w k`, new note `Esc w K`): jot in-place with focus, cycle notes with `[`/`]`, save with Ctrl-S, archive Ctrl-A, delete Ctrl-X. Markdown storage remains under the hood; notes are no longer opened as ordinary editor buffers by default.
- **Sticky → agent brief:** if the sticky pad is open when you start `Esc w a`, the active note is attached to the work packet (`sticky_ids` + bounded `sticky_brief`) and the fake agent receipts a sticky-brief notice (workflow-style context without embedding a workflow runtime).
- **Sticky checklist fan-out (S2):** parse open `- [ ]` / `* [ ]` lines on the pad; `Esc w C` / `:agent-checklist` runs a capped (6) fake agent job per item; after REVIEW, `Esc w Y` applies `[x]` checkmarks to the sticky (human confirm only).
- **Maintainer sticky workflows (S3):** `.grok/workflows/sticky-pad-review.rhai` and `sticky-anchor-audit.rhai` for Grok Build fan-out review/audit of the sticky subsystem (excluded from the crates.io package).
- Agent host-auth readiness: config fields `profile`, `auth_check_argv`, `required_env` (names only); `wscrpt --health` reports agent mode/auth markers without storing secrets; docs in `docs/AGENT_AUTH.md`.
- Dual-lane contributor contract for concurrent Stickies vs Agents work (`docs/LANES.md` and handoffs).

### Changed

- Removed the separate **Agent Activity** popup. Receipt and status live only in the bottom Agents dashboard — one surface, one function.

## [0.2.1] - 2026-08-01

### Added

- First-run help: a one-time overlay on the first launch, remembered in the XDG state directory and never written into the workspace. `WSCRPT_SKIP_FIRST_RUN_HELP=1` skips it for automation.
- Daily LSP visibility: the footer shows the attached server and its state, errors name the discovered binary and the exact config step to authorize it, and `--print-default-config` emits ready-to-uncomment `[[language_servers]]` blocks for servers found on PATH.
- Optional `format_on_save` (default off; explicit `Esc c f` formatting is unchanged).
- The Workspace Info route snapshot reports the selected OSC 52 clipboard transport.
- Stickies v1: Markdown notes with TOML front matter. Personal notes live under `$XDG_STATE_HOME/wscrpt/stickies/<workspace-key>/`; team notes under `.wscrpt/stickies/`. Layout stays out of the workspace. `Esc w k` / `:stickies` lists notes; Enter opens; `A` archives; `Esc w K` / `:new-sticky` creates a personal sticky anchored to the current file (or workspace). Bodies sanitize terminal escapes; invalid files are skipped with partial-list warnings.
- Trusted local Git mutations return as a deliberately smaller surface: `Esc v S` / `:stage-current`, `Esc v U` / `:unstage-current`, and `Esc v c` / `:commit-staged`. They operate only on the current saved file or already staged index, run one at a time on bounded non-interactive workers, require trust every run because filters/hooks may execute, and refresh Git status after every outcome. Branch, network, discard/reset/clean, arbitrary-path, and signed-commit workflows remain in the workspace shell.
- Agent-native contracts (W0): work packets with explicit path scope and authority, bounded agent events, Sticky and review-packet types, a host-local `AgentCoordinator` admission point (workspace id + generation + monotonic sequence), and a deterministic fake agent. Stale, oversized, out-of-scope, replayed, and cancelled events are rejected before they can affect the current workspace. No ACP process, TUI drawer, or filesystem mutation yet — orchestration surface only.
- Optional native iPad terminal + player preview harness and host preview sidecar live in-tree under `clients/` and `previewd/` for contributors; they are excluded from the Rust crate package and are not required to install or run `wscrpt`.

### Changed

- Clipboard copies inside tmux now wrap OSC 52 in tmux's DCS passthrough envelope, reaching the outer terminal (Blink) directly instead of depending on tmux's `set-clipboard` forwarding. On tmux 3.3+ this requires `allow-passthrough on`; `wscrpt --health` says so inside tmux.
- Renderer paint reductions for remote links: differential style emission per span and allocation-free grapheme display cut per-frame output bytes measurably over SSH/mosh.
- Buffer tabs use a fixed position-indexed color scale, so switching buffers never recolors the header.
- The line-oriented action-layer shortcuts now use `Esc l` to select the current line or expand an existing selection to whole lines, and `Esc L` to toggle line numbers.
- Host support is explicit: macOS and Linux remain the tested remote-host contract, WSL 2 is the practical Windows route, and native Windows is not claimed without terminal/process-tree CI.
- Leaving the Action layer now arms one revision-driven acknowledgement: only the first successful document edit shows a steady 360 ms `EDIT*`/cursor cue; navigation, buffer changes, and rejected/read-only edits do not consume it.

### Fixed

- Mouse clicks with the project sidebar visible landed sidebar-width cells left of the pointer and used the wrong width for wrapped-scroll metrics. Hit-testing now mirrors the renderer's sidebar layout; clicks inside the sidebar region are ignored.
- `scripts/verify.sh` no longer hardcodes the package version; the gate follows `Cargo.toml`.

## [0.2.0] - 2026-07-28

This release narrows `wscrpt` to a safer remote editor/IDE core before further feature work.

### Kept

- UTF-8 editing, atomic saves, external-change protection, buffers, workspace tree/sidebar, Quick Open, project search, in-buffer literal/regex replacement, tasks with the existing trust gate, recovery journals, session restore, and the full-screen workspace shell.
- Read-only Git status, changed-file navigation, diffs, log, commit inspection, file history, HEAD, blame, branches, and upstream information.
- LSP diagnostics, completion, hover, definition, references, symbols, restart/logging, and single-document formatting.

### Removed commands and replacements

| Removed command | Former route | Removed colon aliases | Replacement |
| --- | --- | --- | --- |
| Embedded Terminal (`task.terminal-live`) | `Esc t T` | `terminal-live`, `term-live`, `shell-live`, `terminal-embed`, `embedded-terminal` | Use `Esc t t` or `:terminal` for a real full-screen workspace shell. |
| Terminal Split (`task.terminal-split`) | `Esc t v` | `terminal-split`, `term-split`, `shell-split`, `terminal-pane`, `term-pane` | Use the full-screen workspace shell. |
| Terminal Panel (`task.terminal-panel`) | `Esc t p` | `terminal-panel`, `term-panel`, `shell-panel`, `pty-panel` | Use Workspace Info and Task Output for editor context; use the workspace shell for terminal work. |
| Terminal PTY Probe (`task.terminal-probe`) | `Esc t P` | `terminal-probe`, `term-probe`, `shell-probe`, `pty-probe` | Use `wscrpt --health`, `wscrpt --input-diagnostics`, or the workspace shell. |
| Stop Embedded Terminal (`task.terminal-stop`) | `Esc t S` | `terminal-stop`, `term-stop`, `shell-stop`, `pty-stop` | Exit or interrupt the foreground program in the full-screen shell. |
| Replace in Files (`workspace.replace`) | `Esc w p` | none | Use a trusted task or shell tool for project-wide rewrites; use `Esc R` for the active buffer. |
| LSP Code Actions (`code.actions`) | `Esc c a` | none | Apply focused edits manually or through a trusted task/shell tool. |
| LSP Rename Symbol (`code.rename`) | `Esc c n` | none | Use local editing or a trusted external refactoring tool. File Rename (`Esc w m`) remains. |
| Stage Current File (`vcs.stage-current-file`) | `Esc v S` | `stage-current`, `stage-current-file`, `stage-file` | Run `git add` in the workspace shell. |
| Unstage Current File (`vcs.unstage-current-file`) | `Esc v U` | `unstage-current`, `unstage-current-file`, `unstage-file` | Run the desired Git restore/reset command in the workspace shell. |
| Commit Staged Changes (`vcs.commit-staged`) | `Esc v c` | `commit`, `commit-staged`, `git-commit` | Run `git commit` in the workspace shell. |
| Switch Branch (`vcs.checkout`) | `Esc v k` | `checkout`, `switch`, `git-checkout`, `git-switch` | Run `git switch` or `git checkout` in the workspace shell. |
| Pull Fast-Forward Only (`vcs.pull`) | `Esc v p` | `pull`, `git-pull` | Run the chosen pull/fetch workflow in the workspace shell. |
| Push Current Branch (`vcs.push`) | `Esc v P` | `push`, `git-push` | Run `git push` in the workspace shell. |
| Explicit-path Git stage/unstage | no key route | `stage PATH`, `unstage PATH` | Run Git in the workspace shell. |

### Changed

- Startup indexing, Git discovery/status, and recovery scanning now begin after terminal initialization on bounded background workers. The first frame does not wait for them.
- Background results carry a workspace identity and per-service generation. Stale results are rejected centrally, and dependent commands explain when data is pending or unavailable.
- `App` remains the runtime façade but now owns grouped UI, project, LSP, task, persistence, and read-only Git state. `workspace` and `config` are private behind narrow accessors.
- Prompt behavior is represented by typed `PromptFlow` metadata for prefixes, input limits, candidate policy, and completion behavior.
- Session format version 2 drops terminal visibility flags. Version 1 is explicitly migrated; unknown future versions are refused with a recoverable startup notice.
- The command registry is the source for the keymap, palette, in-editor reference, and generated [command reference](docs/COMMANDS.md).
- The orphaned production ANSI terminal, PTY, and project-replacement modules were removed.
