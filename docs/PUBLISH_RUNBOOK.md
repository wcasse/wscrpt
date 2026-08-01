# Publish runbook — v0.2.3 (SHIP lane)

Tag **HEAD** as `v0.2.3` after packaging commits land and gates pass.
Do **not** move an existing public tag once it has been published on that commit.

Identity: commits use GitHub noreply `163216174+wcasse@users.noreply.github.com`.

## Preconditions

```sh
cd /path/to/wscrpt
git status                    # clean
gh auth status                # optional; UI works for Releases
scripts/audit-public-source.sh
scripts/audit-public-source.sh --history
scripts/verify.sh
cargo publish --dry-run --locked
```

## Tag and push

```sh
git push origin main
git tag -a v0.2.3 -m "wscrpt 0.2.3"
git push origin v0.2.3
```

If `v0.2.3` already exists on a different commit: **stop**. Ship a new patch.

## GitHub Releases (CLI or UI)

```sh
gh release create v0.2.0 --title "wscrpt 0.2.0" --notes-file docs/releases/v0.2.0.md --verify-tag || true
gh release create v0.2.1 --title "wscrpt 0.2.1" --notes-file docs/releases/v0.2.1.md --verify-tag || true
gh release create v0.2.2 --title "wscrpt 0.2.2" --notes-file docs/releases/v0.2.2.md --verify-tag || true
gh release create v0.2.3 --title "wscrpt 0.2.3" --notes-file docs/releases/v0.2.3.md --verify-tag
```

## Topics + private vuln reporting

```sh
gh repo edit wcasse/wscrpt \
  --add-topic terminal --add-topic editor --add-topic ide \
  --add-topic ssh --add-topic mosh --add-topic ipad --add-topic rust
```

Settings → Code security → Private vulnerability reporting → Enable.

## crates.io

```sh
cargo login
cargo publish --dry-run --locked
cargo publish --locked
```

## Post-publish

```sh
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.3 --locked --force
wscrpt --version && wscrpt --health
```
