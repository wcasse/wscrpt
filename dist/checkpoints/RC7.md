# w.scrpt 0.1.0 RC7 checkpoint

Created on 2026-07-28 from `/path/to/projects/w.scrpt` after the
final adversarial closure pass for preview-gated project-wide literal Replace
in Files. The workspace was not a Git repository, so branch and commit identity
are unavailable. RC7 supersedes RC6 as the release candidate because two final
audit findings were closed after RC6's binary was sealed.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc7-aarch64-apple-darwin`
  - Size: 2,650,176 bytes
  - SHA-256: `cd1cc669c6c3c87e060b971a3266d57318aaa7690fea0431fcc973f5706d4465`
- `wscrpt-0.1.0-rc7.crate`
  - Size: 368,591 bytes
  - SHA-256: `aa03639a4397a87f9ff547398308dbbde3de919660a8ee565398749036fdb07b`

RC7 closure over RC6:

- Replace confirmation is now an allowlist: only an unmodified key or exactly
  Shift may confirm `y`/`Y` or stale-preview `r`/`R`. Control, Alt, Super,
  Meta, Hyper, and combinations cannot apply or rebuild the plan.
- Each complete replace validator now brackets its alias-resolution and
  descriptor-relative all-source read batch with root-identity validation at
  both entry and exit. The validator runs before staging and again immediately
  before publication.
- Permanent regressions cover Control/Alt/Super/Meta/Hyper refusal, Shift-only
  confirmation, complete unmatched disk/open-source staleness, negative-cache
  alias retargeting, mixed-case project ordering, malformed/detached manifests,
  and pre-flatten open-source bounds.
- An independent adversarial re-audit returned PASS with no remaining release
  blocker in the Replace in Files lane.

Feature and safety boundary:

- `Esc w p` opens a find prompt, replacement prompt, bounded scan, and frozen
  `REPLACE PREVIEW`. Replacement is exact, case-sensitive, non-overlapping,
  and explicit; empty find is refused and empty replacement deletes matches.
- Open indexed buffers are authoritative, including unsaved text. Apply
  publishes dirty, per-buffer undoable changes and restores the origin buffer;
  it never writes disk until an explicit Save or Save All.
- The plan retains an exact, ordered source manifest for every scanned indexed
  file, including no-match sources. Every open editor state and unopened raw
  byte snapshot is revalidated twice before publication. Any stale, unsafe,
  ambiguous, or partial source refuses the whole operation with zero mutation.
- Current identities for every open file editor are strictly resolved before
  cache decisions. Positive/negative cache changes, symlink retargets, duplicate
  aliases, late opens, closed buffers, unsafe roots, and source type changes fail
  closed.
- Bounds: 4 KiB find text, 64 KiB replacement text, 256 open-buffer overrides,
  64 MiB aggregate open-buffer text, 8 MiB per indexed source, 128 MiB aggregate
  scan and exact source-manifest retention, 256 changed files, 16,384 matches per
  file, 65,536 matches total, 64 MiB resulting text, 64 MiB inserted replacement
  payload, 16 MiB engine preview payload, and a separately bounded UI label copy.
- Project search uses descriptor-relative indexed reads, before/after root
  validation, visible partial marking for unsafe/unreadable/binary/invalid
  sources, cap-plus-one omitted-match detection, and generation cancellation.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test --all-targets --all-features --locked -- --nocapture`: Cargo
  reported 531 passed, 1 ignored, and 0 failed. The library suite contained 519
  cases (518 passed and 1 intentionally ignored subprocess helper); 5 binary
  and 7 real Unix PTY cases passed. The remaining reported pass was the tmux
  harness's explicit self-skip because isolated socket creation returned
  `Operation not permitted`, so tmux was not exercised.
- `cargo test --doc --all-features --locked -- --nocapture`: 3 compile-fail
  documentation tests passed.
- Focused replacement gates: `workspace_replace` 20/20 and replace engine
  16/16.
- `cargo build --release --locked --offline`: passed.
- `cargo package --locked --offline --allow-dirty`: verified and packaged 37
  files (1.8 MiB, 360.0 KiB compressed).
- The extracted package's `src`, `tests`, README, QA matrix, and original Cargo
  manifest were byte-for-byte equal to the release workspace inputs.
- The extracted package built in a fresh target directory with locked offline
  dependencies. A second locked offline `cargo install` into an isolated root
  succeeded. The installed binary reported `w 0.1.0`; `--health` and
  `--print-default-config` completed. That local health run reported
  `transport=local/unknown`, `tmux=no`, and `TERM=dumb`.
- The workspace, packaged-source, and isolated-install release binaries were
  each 2,650,176-byte arm64 Mach-O executables. Normal macOS link output used a
  unique LC_UUID and derived ad-hoc signature per build. After removing the
  signatures from disposable comparison copies, bytewise comparisons differed
  only in the 16 LC_UUID bytes at offsets 1945-1960.

IDE direction:

- RC7 is a credible streamlined IDE foundation: editing, transient project
  navigation/search/replace, LSP code intelligence, diagnostics and Problems,
  trusted tasks, bounded Git primitives, recovery/session continuity, and a
  full-screen workspace-shell handoff are connected.
- “Streamlined” is the interaction surface, not the capability ceiling. The
  next high-value slices are cancellable background index/Git validation and
  real-iPad input diagnostics; bounded polyglot LSP continuity; then richer
  run/test/debug workflows. These are not RC7 claims.

Open boundaries:

- The strong descriptor-relative traversal and commit guarantee is Unix/macOS
  specific. The documented non-Unix fallback retains pathname TOCTOU risk.
- Confirmation performs two synchronous full-source revalidations. At maximum
  caps that can read about 256 MiB across up to 100,000 file reads; a future
  cancellable background validation phase would improve remote responsiveness.
- The tmux harness self-skipped on this restricted host. No exercised tmux
  route follows from its passing harness result.
- This checkpoint has source, validator, package, isolated-install, and local
  real-PTY evidence only. It is not approved on a real iPad, Magic Keyboard,
  Blink Shell, SSH/mosh route, tmux session, OSC 52 clipboard path,
  touch/trackpad path, or live language-server setup. The exact hardware route
  remains the open gate in `docs/IPAD_BLINK_QA.md`.
