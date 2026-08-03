# wscrpt — agent instructions

Standing rules for **any** automated assistant (any model, any harness) and for
humans doing the same jobs. This file is model-agnostic: no vendor, product, or
CLI brand is privileged here.

Remote-first terminal IDE (Rust, single binary) optimized for iPad + Magic
Keyboard over Blink/SSH/mosh/tmux. Before editing code, read
[docs/CONTRIBUTOR_MAP.md](docs/CONTRIBUTOR_MAP.md) — it maps every concern
(keybindings, save, search, tasks, Git, LSP, render, session, recovery, CLI)
to its module so you do not have to read all of `src/app.rs`.

Owner vs assistant split, lanes, release ceremony, and copy-paste playbooks:
[docs/MAINTAINER_OPS.md](docs/MAINTAINER_OPS.md).

## Gates

- Full gate (must pass before any PR/release): `scripts/verify.sh`
  (fmt → clippy `-D warnings` → tests → doc tests → package → isolated
  install → binary probes). CI runs it on macOS + Linux + MSRV 1.88.
- Quick loop: `cargo fmt --all && cargo clippy --all-targets --all-features
  --locked -- -D warnings && cargo test --all-targets --all-features --locked`
- tmux smoke test is a hard failure when `WSCRPT_REQUIRE_TMUX=1` (Linux CI).
- After changing `src/keymap.rs::COMMANDS`, regenerate:
  `cargo run --locked -- --print-command-reference > docs/COMMANDS.md`
  (a unit test fails on drift).

## House rules

- `src/app.rs` is intentionally large; do **not** dump new subsystems there.
  Prefer a focused module plus thin wiring in `App`. Modularization is
  deliberately deferred — do not start it as a drive-by.
- No new dependencies without maintainer discussion. No async runtime, no
  serde_json, no tree-sitter — these are deliberate absences.
- Mutation boundary is policy: no project-wide replace, LSP rename/code
  actions, in-editor Git mutation, or embedded terminals. See
  docs/ARCHITECTURE.md and the 0.2 CHANGELOG before adding surface.
- Subprocesses are argv vectors only, never shell strings. Executables come
  only from user-global config (LSP) or trust-gated `.wscrpt/tasks.toml`.
- Everything is bounded (queues, payloads, caps) and partial results are
  labeled, never silent. Match that style.
- User-visible changes go in `CHANGELOG.md` under `[Unreleased]`.
- Pushed tags never move; fixes after a public tag ship as a new patch
  version.
- Do not `git push`, tag, force-push, publish crates, or create GitHub
  Releases unless the maintainer explicitly ordered that step. Default:
  stop after a green `scripts/verify.sh` and a reviewable summary.

## Concurrent work

When two assistants (or humans) work at once, follow [docs/LANES.md](docs/LANES.md).
Do not edit another lane’s exclusive modules or shared-file regions without a
handoff.

## Release

See [docs/RELEASING.md](docs/RELEASING.md), [docs/OPEN_SOURCE_CHECKLIST.md](docs/OPEN_SOURCE_CHECKLIST.md),
and [docs/MAINTAINER_OPS.md](docs/MAINTAINER_OPS.md). The human iPad/Blink pass
([docs/IPAD_BLINK_QA.md](docs/IPAD_BLINK_QA.md)) gates release tags; automation
never approves remote typing feel, Escape delivery, reconnect, or terminal
cleanup.

## Public install honesty

Strangers install from a **pushed, immutable git tag** (see README). Do not
change the public install pin without updating README, CHANGELOG, release notes,
and [docs/STATUS.md](docs/STATUS.md) together.
