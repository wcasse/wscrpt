# Changelog

## [Unreleased]

## [0.2.1] - 2026-07-29

### Added

- First-run help: a one-time overlay on the first launch, remembered in the XDG state directory and never written into the workspace. `WSCRPT_SKIP_FIRST_RUN_HELP=1` skips it for automation.
- Daily LSP visibility: the footer shows the attached server and its state, errors name the discovered binary and the exact config step to authorize it, and `--print-default-config` emits ready-to-uncomment `[[language_servers]]` blocks for servers found on PATH.
- Optional `format_on_save` (default off; explicit `Esc c f` formatting is unchanged).
- The Workspace Info route snapshot reports the selected OSC 52 clipboard transport.

### Changed

- Clipboard copies inside tmux now wrap OSC 52 in tmux's DCS passthrough envelope, reaching the outer terminal (Blink) directly instead of depending on tmux's `set-clipboard` forwarding. On tmux 3.3+ this requires `allow-passthrough on`; `wscrpt --health` says so inside tmux.
- Renderer paint reductions for remote links: differential style emission per span and allocation-free grapheme display cut per-frame output bytes measurably over SSH/mosh.
- Buffer tabs use a fixed position-indexed color scale, so switching buffers never recolors the header.

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
