# w.scrpt 0.1.0 RC8 checkpoint

Created on 2026-07-28 from `/path/to/wscrpt` after
broadening task-output Problems into a more useful IDE run/test feedback path.
The workspace was not a Git repository, so branch and commit identity are
unavailable. RC8 supersedes RC7 as the current release candidate.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc8-aarch64-apple-darwin`
  - Size: 2,650,176 bytes
  - SHA-256: `e054748dcb64e37ffda5eee047885de9c009d2c3374b53f395560eb29a2a6cf5`
- `wscrpt-0.1.0-rc8.crate`
  - Size: 369,955 bytes
  - SHA-256: `74a35283242d1cfaeda8e06e65a13ffdd275b646d6662bd7a06c885ea9644136`

RC8 closure over RC7:

- Task-derived Problems now parse common TypeScript/Visual Studio rows of the
  form `path(line,column): message`.
- Task-derived Problems now parse ESLint-style two-line output where a file path
  is followed by `line:column severity message` rows.
- Severity parsing accepts bare severity words followed by whitespace, so
  `error message` and `warning message` rows are classified instead of shown as
  unknown generic problems.
- The existing resolver boundary is unchanged: every parsed path still
  canonicalizes relative to the task working directory, must be an existing
  regular file inside the workspace root, is bounded by existing path/message/
  candidate/result limits, and is revalidated again on navigation.
- The iPad-facing UI is unchanged. `Esc c p` remains the single searchable
  Problems picker; this is an IDE capability expansion without a permanent
  panel.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test task_problem::tests --lib --locked`: 14 passed.
- `cargo test --all-targets --all-features --locked -- --nocapture`: Cargo
  reported 533 passed, 1 ignored, and 0 failed. The library suite contained
  521 cases (520 passed and 1 intentionally ignored subprocess helper); 5
  binary and 7 real Unix PTY cases passed. The remaining reported pass was the
  tmux harness's explicit self-skip because isolated socket creation returned
  `Operation not permitted`, so tmux was not exercised.
- `cargo test --doc --all-features --locked`: 3 compile-fail documentation
  tests passed.
- `cargo build --release --locked --offline`: passed.
- `cargo package --locked --offline --allow-dirty`: verified and packaged 37
  files (1.8 MiB, 361.3 KiB compressed).

Open boundaries:

- Task-derived Problems are still snapshots from the latest retained task
  output, not live diagnostics or underlines. Rerun the task to refresh them.
- Unsupported tool-specific diagnostic formats can still be invisible until
  added explicitly.
- The tmux harness self-skipped on this restricted host. No exercised tmux
  route follows from its passing harness result.
- This checkpoint has source, validator, package, and local real-PTY evidence
  only. It is not approved on a real iPad, Magic Keyboard, Blink Shell,
  SSH/mosh route, tmux session, OSC 52 clipboard path, touch/trackpad path, or
  live language-server setup. The exact hardware route remains the open gate in
  `docs/IPAD_BLINK_QA.md`.
