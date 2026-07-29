# w.scrpt 0.1.0 RC4 checkpoint

Created on 2026-07-28 from `/path/to/projects/w.scrpt` after the
LSP lifecycle, synchronization, workspace-edit atomicity, and boundedness
seal. The workspace was not a Git repository, so there is no branch or commit
identity for this checkpoint.

Host toolchain: `aarch64-apple-darwin`, Rust 1.95.0.

Artifacts:

- `w-0.1.0-rc4-aarch64-apple-darwin`
  - SHA-256: `2f36fe25325564085d9b029d0cb66f84196473e09b5bb27f5a116a1078fda5eb`
- `wscrpt-0.1.0-rc4.crate`
  - SHA-256: `6e323f65591f000c28d768e1f2147747b7227d003c6c5e54598b39febdac67f6`

Feature and safety boundary:

- LSP document synchronization accepts negotiated full or incremental modes.
  Incremental-only servers receive ordered whole-document replacements whose
  ranges use the exact previous UTF-16 end, including astral scalars.
- Up to 64 independently versioned documents remain synchronized with bounded
  fair background work, stable incarnations, explicit lifecycle teardown, and
  bounded per-poll event count plus retained-byte work.
- Diagnostics aliases (`localhost`, dot segments, and percent spelling) are
  normalized in memory against open canonical identities. The protocol thread
  performs no filesystem I/O for server-supplied diagnostic URIs.
- Completion, code-action, response-correlation, disabled-action, malformed
  JSON-RPC, and malformed edit shapes fail closed rather than silently
  degrading into another operation.
- Multi-file workspace edits preflight and revalidate every target before any
  buffer is appended or changed. Source, replacement, result, retained-data,
  failed-candidate work, candidate count, and document-inspection bounds are
  explicit.
- Disk-backed edit and ordinary document reads validate descriptor metadata
  and use nonblocking/no-follow Unix opens. FIFO/device/symlink-swap targets
  are refused without hanging the interactive process.
- Workspace editor IDs use an O(1) index whose identity cannot be replaced or
  invalidated through a forgotten safe mutable guard.

Recorded host proof:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo test --locked --all-targets`: Cargo reported 433 passed, 1 ignored,
  and 0 failed. The library suite contained 423 cases (422 passed and 1
  intentionally ignored subprocess helper); 5 binary and 5 real Unix PTY
  cases passed. The remaining reported pass was the tmux harness's explicit
  self-skip because isolated socket creation was denied, so tmux was not
  exercised.
- Optimized packaged-source build, offline package verification, and isolated
  offline install: passed. The installed executable was byte-identical to the
  packaged-source release binary recorded above.
- Installed `w --version`, `w --health`, and `w --print-default-config`:
  passed. The recorded health route was local/unknown, outside tmux, with
  `TERM=xterm-256color` and locale `C.UTF-8`.
- Independent adversarial review found no remaining macOS/Unix safety blocker.

Open boundaries:

- A position-scoped Definition/Hover/References/Code Action response can still
  arrive after a cursor-only keyboard or mouse move; document mutation remains
  stale-gated, but cursor-intent cancellation is scheduled for the next lane.
- Descendant process-tree cleanup is proven and claimed on Unix/macOS, not on
  Windows.
- `Esc w t` still reuses quick-open in this checkpoint; the transient
  hierarchical explorer begins after this immutable safety boundary.

This checkpoint has validator and local PTY evidence only. It is not approved
on a real iPad, Blink Shell, SSH/mosh route, tmux session, OSC 52 path, or live
language-server setup.
