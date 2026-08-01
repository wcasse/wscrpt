# w.scrpt 0.1.0 RC9 checkpoint

Created on 2026-07-28 from `/path/to/wscrpt` after
adding a terminal input diagnostics route for real iPad/Blink validation. The
workspace was not a Git repository, so branch and commit identity are
unavailable. RC9 supersedes RC8 as the current release candidate.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc9-aarch64-apple-darwin`
  - Size: 2,666,896 bytes
  - SHA-256: `32b8ffa4ba48b73334ea915810e442a39cfbcf92b1dc9f503d1f8f3de93dca0e`
- `wscrpt-0.1.0-rc9.crate`
  - Size: 372,795 bytes
  - SHA-256: `b17704cd1de333cd73f6325506b180f12f72f56342b7c0fc5d78f36a8b2c21d5`

RC9 closure over RC8:

- Added `w --input-diagnostics`, a raw-terminal diagnostic mode that prints
  decoded terminal input events and exits on `Ctrl-G` or `Ctrl-C`.
- The diagnostic route records the current terminal route summary, including
  `TERM`, detected transport, and tmux state.
- Diagnostic output covers decoded key code/modifiers/kind/state, bracketed
  paste byte and character counts with escaped text, resize dimensions, mouse
  events, focus events, and other crossterm event variants.
- The route enables bracketed paste but does not enter alternate screen, hide
  the cursor, or start the editor UI. Terminal raw mode and bracketed paste are
  restored by an RAII guard on exit.
- The iPad QA matrix now includes A08 as the explicit hardware transcript gate
  for Escape, `Ctrl-K`, `Ctrl-G`, arrows, Shift/Option/Command-modified keys,
  native multiline paste, and resize.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test --bin w --locked`: 6 passed, including input diagnostics event
  formatting and exit-key behavior.
- `cargo test --test unix_pty input_diagnostics_reports_keys_paste_resize_and_restores_modes --locked -- --nocapture`:
  1 passed.
- `cargo test --locked --test unix_pty -- --nocapture`: 8 real Unix PTY cases
  passed, including the input diagnostics route.
- `cargo test --locked --test tmux_smoke -- --nocapture`: harness passed as an
  explicit self-skip because isolated socket creation returned `Operation not
  permitted`, so tmux was not exercised on this host.
- `cargo test --all-targets --all-features --locked`: Cargo reported 535
  passed, 1 ignored, and 0 failed. The library suite contained 521 cases (520
  passed and 1 intentionally ignored subprocess helper); 6 binary and 8 real
  Unix PTY cases passed. The remaining reported pass was the tmux harness's
  explicit self-skip.
- `cargo test --doc --all-features --locked`: 3 compile-fail documentation
  tests passed.
- `cargo build --release --locked --offline`: passed.
- `cargo package --locked --offline --allow-dirty`: verified and packaged 37
  files (1.8 MiB, 364.1 KiB compressed).
- Checkpoint binary `--version`, `--health`, and `--print-default-config`:
  passed. The local health route reported `TERM=dumb`, `transport=ssh`, and
  `tmux=no`; this is not a hardware route transcript.

Open boundaries:

- `w --input-diagnostics` provides objective hardware-route evidence, but the
  required real iPad/Blink/mosh/SSH transcript has not yet been captured.
- Command-modified keys may be reserved, rewritten, or dropped by iPadOS or
  Blink; the diagnostic transcript is the authority for those chords.
- The tmux harness self-skipped on this restricted host. No exercised tmux
  route follows from its passing harness result.
- This checkpoint has source, validator, package, local real-PTY, and installed
  binary health evidence only. It is not approved on a real iPad, Magic
  Keyboard, Blink Shell, SSH/mosh route, tmux session, OSC 52 clipboard path,
  touch/trackpad path, or live language-server setup. The exact hardware route
  remains the open gate in `docs/IPAD_BLINK_QA.md`.
