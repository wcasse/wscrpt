# Release checkpoints

Markdown records and `SHA256SUMS` document historical release-candidate evidence.

**Binary and crate payloads are not part of the published source tree.** They are gitignored. To archive them for provenance, follow [docs/RELEASING.md](../../docs/RELEASING.md): upload to a durable GitHub Release, verify downloads, then remove local payloads while keeping Markdown + checksums.

Do not commit large binaries to the default branch.
