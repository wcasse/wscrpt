# w.scrpt 0.1.0 RC5 checkpoint

Created on 2026-07-28 from `/path/to/projects/w.scrpt` after the
streamlined full-IDE workspace explorer, explicit snapshot refresh, and
runtime path-identity safety seal. The workspace was not a Git repository, so
there is no branch or commit identity for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc5-aarch64-apple-darwin`
  - SHA-256: `deccf7952dc881d2923a35b22a53a64a12b29a99c5c4678ffe7cba8c6a0608e3`
- `wscrpt-0.1.0-rc5.crate`
  - SHA-256: `ce0cdda90a52d1002be8fee69340c8597796f30743ce10d75274262b06d7f36a`

Feature and safety boundary:

- `Esc w t` is a transient hierarchical workspace explorer rather than a
  persistent sidebar. It supports bounded expansion/collapse, active-file
  selection, flat fuzzy filtering, safe filename presentation, and visible
  errors without taking focus away from editing.
- `Esc w R`, `:refresh`, and the command palette rebuild one shared bounded
  snapshot for the explorer, Quick Open, and project search. Failed refreshes
  retain the prior snapshot and report the failure; open buffers are not
  reloaded.
- The explorer uses an immutable tree snapshot independent of the text-search
  index. It includes regular binary files and empty non-ignored directories,
  while omitting symlinks, special files, ignored paths, and unreadable
  entries. Result, path, depth, scan, query, expansion, and visible-node caps
  are explicit and truncation is labeled `Partial`.
- One bounded canonical in-root active path may be revealed outside the
  snapshot for a new, ignored, or capped-out buffer. Positive and negative
  identities are cached across runtime opens, recovery, LSP/task navigation,
  code-action admission, reload, Save As, ordinary save, force-save, Save All,
  and jump restoration. Lexical snapshot membership is never accepted as
  identity proof.
- Missing file suffixes are admitted only below a canonical existing prefix.
  Dangling ancestor/final symlinks, later outside-root resolution, alias
  duplication, stale directory-to-file types, and root-symlink replacement
  are covered by permanent regressions and fail closed for explorer identity.
- On Unix/macOS, project traversal, refresh, and tree-file commit use held
  descriptors with `O_NOFOLLOW`, `O_DIRECTORY`, `O_NONBLOCK`, descriptor
  metadata checks, and bounded reads. A replacement root symlink makes refresh
  fail while the previous snapshot remains usable.
- Starting a filter selects the highest-ranked result instead of inheriting a
  hierarchy row number. An active outside-snapshot match reserves one of the
  100 result slots, and both index and result-cap loss are reported honestly.
- Opening the first explorer file from the pristine untitled buffer no longer
  records a dead jump origin. Cursor-only keyboard and mouse movement also
  cancels position-scoped LSP UI intent, closing RC4's recorded follow-up.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test --all-targets --all-features --locked -- --nocapture`: Cargo
  reported 482 passed, 1 ignored, and 0 failed. The library suite contained
  471 cases (470 passed and 1 intentionally ignored subprocess helper); 5
  binary and 6 real Unix PTY cases passed. The remaining reported pass was the
  tmux harness's explicit self-skip because isolated socket creation returned
  `Operation not permitted`, so tmux was not exercised.
- `cargo test --doc --all-features --locked`: 3 passed, 0 failed.
- Offline release build, offline package verification, packaged-source release
  build, and isolated offline install: passed. Cargo packaged 36 files (1.6
  MiB, 332.6 KiB compressed). The installed executable was byte-identical to
  the packaged-source release binary recorded above.
- Installed `w --version`, `w --health`, and `w --print-default-config`:
  passed. The recorded health route was local/unknown, outside tmux, with
  `TERM=xterm-256color` and locale `C.UTF-8`; Git and optional `rg` were found.
- Two independent adversarial reviews found no remaining blocker in the RC5
  explorer/refresh/path-identity scope.

Open boundaries:

- The strong descriptor-relative traversal and commit guarantee is Unix/macOS
  specific. The documented non-Unix fallback retains pathname TOCTOU risk.
- The explorer is a bounded immutable snapshot, not a live watcher; use
  `Esc w R` or `:refresh` after external filesystem changes.
- The tmux harness was explicitly skipped on this restricted host. No tmux,
  SSH, mosh, Blink Shell, OSC 52, or reconnect claim follows from this run.
- This checkpoint has validator and local real-PTY evidence only. It is not
  approved on a real iPad, Magic Keyboard, Blink Shell, SSH/mosh route, tmux
  session, clipboard path, touch/trackpad path, or live language-server setup.
- The capability ceiling remains a full IDE with a streamlined transient UI.
  Project-wide replace with preview, simultaneous polyglot language services,
  and debugger/run-control surfaces remain future lanes, not RC5 claims.
