# w.scrpt 0.1.0 RC6 checkpoint

Created on 2026-07-28 from `/path/to/projects/w.scrpt` after the
preview-gated project-wide literal Replace in Files lane. The workspace was not
a Git repository, so there is no branch or commit identity for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc6-aarch64-apple-darwin`
  - SHA-256: `aa51eb14b287c8d5cb94a623df9d62473208d37fd64a83e18bb27b1c7d219559`
- `wscrpt-0.1.0-rc6.crate`
  - SHA-256: `aa03639a4397a87f9ff547398308dbbde3de919660a8ee565398749036fdb07b`

Feature and safety boundary:

- `Esc w p` adds project-wide literal Replace in Files with a frozen
  `REPLACE PREVIEW`. The flow is find prompt, replacement prompt, bounded scan,
  preview, explicit plain `y`/`Y` apply, stale-only plain `r`/`R` rebuild, and
  `Esc`/`Ctrl-G` cancel.
- Replacement is exact, case-sensitive, and non-overlapping. Empty find is
  refused; empty replacement deletes matches.
- Open indexed buffers are authoritative, so dirty unsaved text participates
  in the preview instead of stale disk text. Successful apply publishes dirty,
  per-buffer undoable editor changes only; it never writes disk until Save or
  Save All.
- The preview retains an exact source manifest for every scanned indexed file,
  including sources with no matches. Confirmation revalidates every open state
  and every unopened raw-byte snapshot before any buffer is appended or
  changed. A late change in a no-match source is therefore stale and refuses
  the entire apply.
- Current open-buffer identity is strictly re-resolved at capture and commit.
  Positive indexed identities, negative cached aliases, late opens through
  symlinks, duplicate aliases, retargeted symlinks, closed buffers, and root
  identity changes fail closed instead of producing partial mutation.
- Planning refuses partial indexes, unreadable/unsafe/type-changed sources,
  binary/control-heavy/invalid UTF-8 text, duplicate or unknown overrides,
  cancellation/supersession, and all cap-plus-one cases. It never exposes an
  Apply action for a retained subset.
- Bounds: 4 KiB find text, 64 KiB replacement text, 256 open-buffer overrides,
  64 MiB aggregate open-buffer text, 8 MiB per indexed source, 128 MiB scanned
  and exact source-manifest retention, 256 changed files, 16,384 matches per
  file, 65,536 matches total, 64 MiB resulting text, 64 MiB inserted
  replacement payload, and 16 MiB engine preview labels plus at most one
  bounded UI copy.
- Project search was also hardened to use descriptor-relative indexed reads,
  root revalidation, visible partial marking for unsafe/unreadable/binary/
  invalid sources, cap-plus-one omitted-match detection, and stronger
  generation cancellation.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test --all-targets --all-features --locked -- --nocapture`: Cargo
  reported 530 passed, 1 ignored, and 0 failed. The library suite contained
  518 cases (517 passed and 1 intentionally ignored subprocess helper); 5
  binary and 7 real Unix PTY cases passed. The remaining reported pass was the
  tmux harness's explicit self-skip because isolated socket creation returned
  `Operation not permitted`, so tmux was not exercised.
- `cargo test --doc --all-features --locked`: 3 passed, 0 failed.
- Focused replacement gates: `cargo test workspace_replace --lib --locked`
  reported 19 passed; `cargo test replace::tests --lib --locked` reported 16
  passed.
- Offline release build and offline Cargo package verification: passed. Cargo
  packaged 37 files (1.8 MiB, 360.0 KiB compressed).
- Packaged-source offline release build and isolated offline install: passed.
  Installed `/private/tmp/wscrpt-rc6.vj7P6D/install/bin/w` reported
  `w 0.1.0`; `--health` and `--print-default-config` also completed.
- The current workspace release binary, packaged-source release binary, and
  isolated installed binary were functionally verified but not byte-identical
  across build roots.

Open boundaries:

- The strong descriptor-relative traversal and commit guarantee is Unix/macOS
  specific. The documented non-Unix fallback retains pathname TOCTOU risk.
- The tmux harness was explicitly skipped on this restricted host. No exercised
  tmux route follows from this run.
- This checkpoint has validator, package, isolated install, and local real-PTY
  evidence only. It is not approved on a real iPad, Magic Keyboard, Blink
  Shell, SSH/mosh route, tmux session, OSC 52 clipboard path, touch/trackpad
  path, or live language-server setup.
- The capability ceiling remains a full IDE with a streamlined transient UI.
  Embedded terminal panes, persistent sidebars, simultaneous polyglot language
  services, debugger/run-control surfaces, and real iPad feel approval remain
  future or hardware-gated lanes, not RC6 claims.
