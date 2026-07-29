# Releasing

## Automated gate

Run the single repository entrypoint from a clean release checkout:

```sh
scripts/verify.sh
```

It enforces formatting, strict Clippy, every target/feature test, documentation tests, package verification, isolated installation from the packaged source, and installed-binary probes for:

```text
wscrpt --version
wscrpt --health
wscrpt --print-default-config
wscrpt --print-command-reference
```

Linux CI sets `WSCRPT_REQUIRE_TMUX=1`. With that setting, a missing tmux executable or unusable isolated tmux server is a failure, not a skipped success. Stable Rust runs on Linux and macOS; Rust 1.88 is the MSRV job.

Regenerate the committed command reference after changing the command registry:

```sh
cargo run --locked -- --print-command-reference > docs/COMMANDS.md
```

The unit suite compares the committed file with the registry-generated result.

## Candidate evidence

For each candidate, record in `dist/checkpoints/`:

- host name, OS/build, architecture, Rust/Cargo version, and tmux version;
- source revision and whether the checkout was clean;
- packaged crate SHA-256 and installed binary SHA-256;
- exact verification commands and pass/fail/skip counts;
- whether tmux ran or skipped, including the reason;
- installed `wscrpt --version`, `wscrpt --health`, and non-interactive probe results.

Install and probe the packaged source, not a different working-tree build. Launch `wscrpt .` on a real PTY before promotion; the Unix PTY integration suite is the deterministic automation for launch/edit/save/resize/cleanup and full-screen shell return.

## Historical checkpoint archive

Provenance Markdown records are never deleted. Before removing any binary or crate payload from `dist/checkpoints`:

1. Verify its SHA-256 against `dist/checkpoints/SHA256SUMS`.
2. Upload every historical payload and the checksum index to a durable GitHub Release archive.
3. Download the uploaded assets into a fresh temporary directory.
4. Verify every downloaded checksum and asset count.
5. Record the release URL, retrieval date, and verification result in a provenance record.
6. Only then remove the archived binary/crate payloads from the active repository, keeping every Markdown record and `SHA256SUMS`.

No archive or deletion is complete merely because the checksum index exists.

## Real hardware gate

Automation does not approve the remote interaction contract. Before release, complete [the iPad/Blink matrix](IPAD_BLINK_QA.md) on:

```text
iPad + Magic Keyboard -> Blink -> mosh -> tmux -> release host
```

Record device, iPadOS, Blink version, transport, tmux state, `TERM`/locale/route variables, source revision, binary hash, and terminal cleanup status. Human approval is required for typing latency, Escape/action delivery, Unicode editing, wrapped navigation, Quick Open, search, completion, task execution, shell handoff, clipboard attempt, reconnect, recovery, and clean exit.
