# Contributing to wscrpt

Thanks for helping. wscrpt is intentionally small: a remote-first terminal editor/IDE optimized for iPad + keyboard sessions over SSH/mosh, not a general-purpose VS Code clone.

## Before you open a PR

1. Read [README.md](README.md) non-goals and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
2. Prefer fixing reliability, safety, or the remote/iPad route over adding surface area.
3. Do not reintroduce removed 0.2 capabilities (embedded terminals, multi-file replace, LSP rename/code actions, in-editor Git mutation) without an explicit design discussion and release-note plan.

## Development

Rust **1.85+** is required.

```sh
cargo fmt --all
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
scripts/verify.sh
```

On Linux CI, `WSCRPT_REQUIRE_TMUX=1` makes a missing or unusable tmux a hard failure. Locally on macOS, install tmux if you want the smoke test to run for real.

Regenerate the command reference after keymap changes:

```sh
cargo run --locked -- --print-command-reference > docs/COMMANDS.md
```

A unit test fails if the committed file drifts from the registry.

## Design preferences

- **Explicit over clever.** Bounded queues, labeled partial results, recoverable errors.
- **Narrow mutation.** Buffer edits, atomic save, safe create/rename/copy, single-document format. Everything else goes through trusted tasks or the full-screen workspace shell.
- **iPad keyboard first.** No dependence on Command-key chords. Escape/action layer must never time out.
- **User-owned executables.** Language servers only from `~/.config/wscrpt/config.toml`. Tasks only from `.wscrpt/tasks.toml` with a trust gate.

## Commit and PR hygiene

- Keep diffs focused; avoid drive-by refactors in the same PR as behavior changes.
- Do not commit `target/`, `scratch/`, logs, or binary payloads under `dist/checkpoints/`.
- Mention any intentional boundary change in `CHANGELOG.md`.

## Security-sensitive changes

Task runners, LSP process launch, shell handoff, clipboard (OSC 52), and filesystem traversal deserve extra scrutiny. See [SECURITY.md](SECURITY.md).
