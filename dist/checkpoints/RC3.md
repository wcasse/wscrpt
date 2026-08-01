# w.scrpt 0.1.0 RC3 checkpoint

Created on 2026-07-28 from `/path/to/wscrpt` after the
Workspace Symbols lane and its release-blocking race/safety audit. The
workspace was not a Git repository, so there is no branch or commit identity
for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc3-aarch64-apple-darwin`
  - SHA-256: `a5b90eabbdbf09704a97b3040d1afaad07094bafdf171eb481ca66496327878a`
- `wscrpt-0.1.0-rc3.crate`
  - SHA-256: `bd72e834424fd837ccb04ea36d983726a3a49e52db971cbdacc9f3b49eafea5c`

Feature boundary:

- `Esc g w` sends one bounded query to the language server selected by the
  active file, then opens a fixed locally filtered Workspace Symbols picker.
- Range-bearing file results navigate with Jump Back/Forward provenance;
  malformed, range-less, non-file, oversized, expired, cancelled, stale, and
  wrong-service results are refused or skipped visibly.
- Workspace-symbol fields, URIs, query/filter text, inspected candidates,
  protocol positions, and pending requests have explicit safety ceilings.
- A new UI action invalidates older LSP UI correlations. Explicit cancellation
  emits idempotent `$/cancelRequest` while retaining terminal-response
  correlation inside the protocol client.
- Candidate construction performs no per-result filesystem canonicalization;
  a selected location is validated before activation and again before commit.
- Cargo packaging excludes checkpoint artifacts, preventing recursive release
  growth.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets -- --nocapture`: Cargo reported 322
  passed, 1 ignored, and 0 failed. The library suite contained 312 cases (311
  passed and 1 intentionally ignored subprocess helper); 5 binary and 5 real
  Unix PTY cases passed. The remaining pass was the tmux harness's explicit
  self-skip because isolated socket creation was denied, so tmux was not
  exercised.
- Optimized build, offline package verification, and isolated offline install:
  passed. The installed executable was byte-identical to the release binary.
- Installed `w --version`, `w --health`, and `w --print-default-config`:
  passed. The recorded health route was local/unknown, outside tmux, with
  `TERM=xterm-256color` and locale `C.UTF-8`.

This checkpoint has validator and local PTY evidence only. It is not approved
on a real iPad, Blink Shell, SSH/mosh route, tmux session, OSC 52 path, or live
language-server setup.
