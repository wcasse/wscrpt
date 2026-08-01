# Public-source audit

Audit date: 2026-08-01

Scope: the tracked snapshot on the isolated native terminal/player branch, the
Rust package boundary, the preview sidecar, the native iPad client, and Git
history reachable from that branch. No branch was pushed, no tag moved, and no
history was rewritten.

## Current tracked snapshot

- The automated scan found no private-key blocks, common GitHub/AWS/OpenAI/Slack
  token forms, tracked credential files, generated dependency trees, or files
  above the five-megabyte review threshold.
- Absolute developer home paths, named private development hosts, personal test
  usernames, and the personal package/conduct email were replaced with generic
  fixtures or private repository routing.
- The existing repository-owner URL and `CODEOWNERS` handle remain because they
  are operational publishing and security routes. If ownership moves to an
  organization, update those links as one atomic hosting change. This identity
  ledger includes owner-specific URLs in Cargo metadata, README badges and
  install commands, issue/security configuration, release notes, the iPad prep
  script, and `.github/CODEOWNERS`; none were replaced with a fictional owner.
- Numeric loopback addresses and documentation-only RFC 1918 fixture addresses
  remain because they enforce network-boundary behavior. They are not observed
  private route identities; the tracked tip contains no real LAN/Tailscale
  address or SSH credential.
- Historical Markdown checkpoint receipts remain tracked and sanitized. Their
  binary/crate payloads are ignored, and `dist/` is excluded from the Cargo
  package. The preview sidecar and native client are also excluded from the Rust
  crate but remain relevant source-repository components.
- The root project is MIT licensed. Direct Rust, Node, and Swift dependency
  licenses are inventoried in `THIRD_PARTY_NOTICES.md`; no dependency source
  tree is vendored. A compiled iPad distribution still needs the resolved
  upstream license and NOTICE texts in its acknowledgements or bundle.

Run the current-snapshot gate with:

```sh
scripts/audit-public-source.sh
```

## Reachable history boundary

Reachable commit metadata contains a legacy personal address, and older file
snapshots contain the local paths and host aliases removed from the current
tip. A separate recognizable-secret scan found no hit in reachable commits.
The public `v0.2.0` tag and existing clones also make a rewrite a coordinated
publishing decision, not a cleanup side effect.

No history rewrite was authorized or performed. Therefore:

- publishing the current snapshot is suitable when the existing maintainer and
  repository identity are intentionally public;
- publishing this repository as anonymous history is **not** approved;
- anonymous publication should use a reviewed history-free snapshot/new
  repository, or a separately authorized rewrite with tag/clone coordination;
  and
- never use `git push --mirror` from the shared local object database. Local
  experiment refs and unreachable objects are not part of an ordinary branch
  push and should remain that way.

The stricter history check is:

```sh
scripts/audit-public-source.sh --history
```

It is expected to remain red until the owner explicitly chooses and completes
one of the history-publication paths above.

## Publication state at audit time

The existing remote repository was already public, with its default branch at
`905d010`. Local `main` was seven commits ahead, and the preview/native commits
were still local-only. This audit therefore protects the first publication of
the preview/native work; it cannot retract identity or checkpoint material that
was already present in older public history.

## Publication gates still open

- Decide whether reachable author/committer identity is intentionally public.
- Re-run the snapshot scan and full verification from the exact publication
  commit.
- For an iPad binary, bundle resolved third-party license/NOTICE texts and
  complete signing, physical-device, privacy, and network acceptance.
- Keep real-device preview performance and human iPad/Blink typing approval
  separate from source-code publication.
