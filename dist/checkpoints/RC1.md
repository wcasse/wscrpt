# w.scrpt 0.1.0 RC1 checkpoint

Created on 2026-07-28 from `/path/to/wscrpt` before the
next full-IDE feature lane. The workspace was not a Git repository, so there is
no branch or commit identity for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc1-aarch64-apple-darwin`
  - SHA-256: `185d6f1d177b4ab156a2604b9ad2fe3388f1fe3fbef75613ee4044aa93e4f271`
- `wscrpt-0.1.0-rc1.crate`
  - SHA-256: `efe319e0b31ffcc3753d746405ecd9916fa92ccea7abd800708e1a2e9d607831`

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets`: Cargo reported 258 passed, 1 ignored,
  and 0 failed. Five real Unix PTY cases executed. The tmux harness returned
  through its explicit self-skip because isolated socket creation was denied,
  so tmux was not exercised.
- Optimized build, offline package verification, and isolated offline install:
  passed. The installed executable was byte-identical to the release binary.

This checkpoint has validator and local PTY evidence only. It is not approved
on a real iPad, Blink Shell, SSH/mosh route, tmux session, OSC 52 path, or live
language-server setup.
