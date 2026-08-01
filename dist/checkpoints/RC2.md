# w.scrpt 0.1.0 RC2 checkpoint

Created on 2026-07-28 from `/path/to/wscrpt` after the
unified task/LSP Problems lane. The workspace was not a Git repository, so
there is no branch or commit identity for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc2-aarch64-apple-darwin`
  - SHA-256: `6b193c0985cf1c8c42b28e07672b5b86c2e2f07b2d634655877866c31d73a9c2`
- `wscrpt-0.1.0-rc2.crate`
  - SHA-256: `6e303d381c59abbb62db6aa30652e2697e40ea31493437ebaf9939829e17a4ae`

Feature boundary:

- `Esc c p` combines current LSP diagnostics with bounded locations parsed
  from the latest task output.
- Task targets preserve launch-time canonical cwd provenance, remain inside
  the canonical workspace, and are revalidated before navigation.
- rustc scalar columns, generic unknown columns, split UTF-8 pipe reads,
  interleaved stdout/stderr, dropped-output gaps, dirty-buffer reuse, jump
  history, overlay mouse hit-testing, and a 4,096-entry global picker bound
  have deterministic coverage.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets -- --nocapture`: Cargo reported 294
  passed, 1 ignored, and 0 failed. The library suite contained 284 cases (283
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
