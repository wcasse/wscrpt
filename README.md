# wscrpt

[![CI](https://github.com/wcasse/wscrpt/actions/workflows/verify.yml/badge.svg)](https://github.com/wcasse/wscrpt/actions/workflows/verify.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)](Cargo.toml)

**Remote-first terminal IDE for real development hosts** — especially iPad + Magic Keyboard sessions over Blink, SSH, or mosh.

Ordinary typing stays ordinary. `Esc` (or `Ctrl-K`) opens a **no-timeout** action layer that does not depend on Command-key chords or permanent panel clutter. After Action closes, the first real document edit gets one short, steady `EDIT*`/cursor acknowledgement; navigation and failed edits do not trigger it. Mouse reporting is **off by default** so Blink can keep native touch selection.

Version **0.2** is a deliberate core: editing, workspace navigation/search, trusted tasks, recovery, sessions, Git inspection plus three trusted local mutations, bounded LSP, and a safe full-screen workspace shell. It is not a VS Code clone. See [CHANGELOG.md](CHANGELOG.md).

## Who this is for

- You code on a **real** Linux or macOS host and connect from an iPad (or any thin client) over Blink / SSH / mosh / tmux.
- You want workspace tools (Quick Open, search, tasks, LSP assist, Git inspection plus trusted local stage/commit) without GUI remote desktop.
- You care about **reliable Escape**, reconnect survival, recovery journals, and clean terminal restore.

## Who this is not for

- Local GUI IDE workflows (use VS Code, Zed, etc.).
- Full LSP refactor suites, multi-file automated rewrites, or a full Git client (branch, network, discard/reset/clean, and arbitrary Git remain in the shell).
- Native Windows development hosts. Windows users can run the supported Linux host path in WSL 2; see [host support](docs/HOST_SUPPORT.md).

## Why not vim / VS Code remote / nano?

| Need on iPad → Blink → mosh → host | Typical failure | wscrpt |
| --- | --- | --- |
| Delayed Escape / laggy keys | Modal timeouts dump keys into the buffer | Action layer **never times out** |
| Magic Keyboard, no Command-key IDE chords | Many tools assume desktop modifiers | `Esc` / `Ctrl-K` sequences |
| Touch selection in Blink | Mouse capture fights the client | Mouse **off** by default |
| Untrusted clone + LSP/tasks | Workspace-driven executables | LSP only from **user global config**; tasks **trust-gated** |
| Session drops | Lose work / trash the TTY | Session restore, recovery journals, mode cleanup |

## Install

Rust **1.88** or newer. Supported host: macOS or Linux. WSL 2 is the
low-cost Windows route because it provides the same Linux/OpenSSH/tmux
environment; native Windows remains unvalidated. See
[host support](docs/HOST_SUPPORT.md).

**From GitHub (recommended until crates.io):**

```sh
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.1 --locked
wscrpt --health
```

**From a local checkout:**

```sh
git clone https://github.com/wcasse/wscrpt.git
cd wscrpt
cargo install --path . --locked
wscrpt --health
```

**From crates.io** (when published):

```sh
cargo install wscrpt --locked
```

Development / release gate:

```sh
scripts/verify.sh
cargo run --release -- .
```

`wscrpt` needs interactive stdin/stdout. `--version`, `--health`, `--print-default-config`, and `--print-command-reference` are non-interactive.

## Blink + mosh quickstart

On the **host** (once):

```sh
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.1 --locked
# optional: put a sample task file in each project
mkdir -p .wscrpt && cp /path/to/wscrpt/examples/tasks.toml .wscrpt/tasks.toml
```

On the **iPad**:

1. Open **Blink**.
2. `mosh user@host` (or `ssh`), then attach **tmux** if you use it.
3. `wscrpt .` in your project (or `wscrpt` alone to restore the last session).
4. If keys feel wrong: `wscrpt --input-diagnostics` and work through the [iPad matrix](docs/IPAD_BLINK_QA.md).
5. Prefer UTF-8 locales; `--health` warns when the locale is not UTF-8.

Clipboard: internal yank always works; OSC 52 is optional (`--no-osc52` or config). Inside tmux, wscrpt wraps OSC 52 in tmux's passthrough envelope so it reaches Blink directly — on tmux 3.3+ add `set -g allow-passthrough on` to your tmux config.

## First run

```sh
wscrpt
wscrpt .
wscrpt src/main.rs
wscrpt src/main.rs --project .
```

- A directory is the workspace root.
- A file discovers the nearest ancestor project marker; `--project DIR` overrides it.
- Pathless launch restores the last valid session; `--no-session` disables restore/persistence.
- `--mouse` enables terminal mouse reporting (off by default for Blink).
- `--no-osc52` keeps copy in the editor register only.

The first frame does **not** wait for indexing, Git status, or recovery scanning. Use `Esc w R` after external filesystem changes.

**First run:** help opens once (Esc to dismiss). Reopen anytime with `Esc h`. Footer shows LSP status when a server is authorized or discovered on PATH.

**LSP daily path:** `wscrpt --health` → `wscrpt --print-default-config` (includes PATH discoveries) → uncomment servers in `~/.config/wscrpt/config.toml` → restart. Then `Esc c c` complete, `Esc c h` hover, `Esc c f` format, `Esc c p` problems. Optional `format_on_save = true` formats via LSP before each Save.

## Essential controls

Press and release `Esc`, then type a sequence. Prefixes wait indefinitely. `Ctrl-G` cancels; `Ctrl-L` redraws.

| Sequence | Action |
| --- | --- |
| `Esc s` / `Esc S` | Save current / save all |
| `Esc q` | Quit (dirty-buffer protection) |
| `Esc o` | Quick Open |
| `Esc b` | Buffer switcher |
| `Esc /` / `Esc R` | Find / replace all in buffer |
| `Esc w t` / `Esc w S` | Workspace tree / sidebar |
| `Esc w s` / `Esc w R` | Project search / refresh snapshots |
| `Esc c c` | LSP completion |
| `Esc c p` | Unified LSP/task Problems |
| `Esc t d` / `Esc t r` | Default task / task picker (trust-gated) |
| `Esc t t` | Full-screen workspace shell; exit returns to wscrpt |
| `Esc v s` / `Esc v D` | Read-only Git status / changed-diff picker |
| `Esc v S` / `Esc v U` | Trust-gated stage / unstage current saved file |
| `Esc v c` | Trust-gated commit of already staged changes |
| `Esc ?` / `Esc Space` | Keymap reference / command palette |
| `Esc :` | Colon command line |

Full list: [docs/COMMANDS.md](docs/COMMANDS.md) or `wscrpt --print-command-reference`.

## Capabilities (0.2)

- **Editing:** UTF-8/grapheme movement, soft wrap, selections, undo/redo, find/replace, syntax highlight, LF/CRLF, atomic saves, external-change detection.
- **Workspace:** multi-buffer, Quick Open, tree/sidebar, recent/dirty/closed files, create/rename/copy, project search, outlines/annotations, bookmarks, jump history.
- **Tasks:** `.wscrpt/tasks.toml` argument vectors, trust before every run, bounded output, process-group cancel, Problems extraction. See [examples/tasks.toml](examples/tasks.toml).
- **LSP:** user-global servers only; diagnostics, completion, hover, definition, references, symbols, format (single document).
- **Git:** read-only inspect plus trust-gated, asynchronous `stage-current`, `unstage-current`, and `commit-staged`. Fixed direct arguments only; the UI warns that filters/hooks may run. Branch, network, destructive, and arbitrary Git stay in the workspace shell.
- **Resilience:** session state, recovery journals, bracketed paste, differential redraw, terminal restore on exit/panic.

### Intentional non-goals (0.2)

- Embedded / split terminals inside the TUI → use `Esc t t` / `:terminal`.
- Project-wide replace, LSP rename, code actions, cross-file workspace edits → shell or trusted external tools.
- Git branch/switch, push/pull/fetch, discard/reset/clean, signed commit, and arbitrary path mutation → workspace shell.

## Configuration

```sh
wscrpt --print-default-config
```

Save as `~/.config/wscrpt/config.toml`. Language servers are never enabled by merely opening a project.

## Proof and release

- Host gate: [scripts/verify.sh](scripts/verify.sh) (fmt, clippy `-D warnings`, tests, package, isolated install).
- CI: stable macOS/Linux + Rust 1.88 MSRV ([.github/workflows/verify.yml](.github/workflows/verify.yml)).
- Human gate: [docs/IPAD_BLINK_QA.md](docs/IPAD_BLINK_QA.md) on  
  `iPad + Magic Keyboard → Blink → mosh → tmux → host`.  
  Host prep (agent-friendly): `scripts/ipad-matrix-prep.sh` → short `HUMAN_PASS.md`.

Architecture and packaging rules: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/RELEASING.md](docs/RELEASING.md).  
Host matrix and Windows boundary: [docs/HOST_SUPPORT.md](docs/HOST_SUPPORT.md).
Contributing / security: [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
