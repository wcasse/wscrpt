# Pre-0.2 intake checkpoint

Date: 2026-07-28 PDT

Workspace: `/path/to/projects/w.scrpt`

Git: this workspace has no `.git` metadata, so branch and commit identities are
not available. The immutable Cargo source package below is the intake identity.
All existing RC Markdown, crate, and binary records were left in place.

Host: `GROUDON.local`, `aarch64-apple-darwin`, macOS 26.3.1 (25D771280a).

Toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`, Cargo 1.95.0.

## Immutable intake source

- `dist/checkpoints/wscrpt-0.1.0-pre-0.2.crate`
  - size: 488,586 bytes
  - mode: read-only (`0444`)
  - sha256: `677b4cd333dd266d959fe43eeee949209d667f7c8f98d7f1f6fa880a2d5784eb`
  - packaged before formatting or baseline repair

## Baseline defects repaired before the 0.2 cut

- Normalized existing Rust formatting so `cargo fmt --all -- --check` passes.
- Removed dead syntax-highlighter assignments and other strict-Clippy findings
  without changing supported behavior.
- Made the initial frame immediately eligible for rendering, fixing a race in
  which queued input could quit a tiny terminal before its first frame.
- Updated an embedded-terminal integration assertion to match the existing ANSI
  interpretation behavior.

## Repaired baseline verification

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- `cargo test --all-targets --all-features --locked -- --nocapture`: passed.
  - Library: 678 passed, 1 intentionally ignored subprocess helper.
  - Binary: 6 passed.
  - Unix outer-PTY integration: 9 passed.
  - tmux harness: one Cargo-level pass, but explicitly self-skipped because the
    sandbox denied isolated socket creation (`Operation not permitted`). It is
    not counted as tmux-route proof.
- `cargo test --doc --all-features --locked`: 3 passed.
- `cargo package --locked --allow-dirty --offline`: packaged and verified 48
  files successfully.
- Isolated install from the packaged source: passed.
  - `w --version`, `w --health`, and `w --print-default-config`: passed.
  - installed binary size: 4,377,184 bytes.
  - installed binary sha256:
    `1ee2a6745f12513e7075b98ebde098107dbd560db4661589bf48fd46df264666`.

## Proof boundary

No browser route applies to this terminal application. Real iPad + Magic
Keyboard -> Blink -> mosh -> tmux -> GROUDON acceptance was not exercised and
remains a human hardware gate.
