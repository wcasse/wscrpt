# Publish runbook — v0.2.1 (Strategy A)

Tag **HEAD** as `v0.2.1` after the release-notes fold.
Do **not** move an existing public tag once it has been published on that commit.

Identity: commits use GitHub noreply `163216174+wcasse@users.noreply.github.com`
(personal mailbox was scrubbed from history). Repo ownership remains `wcasse`.

## Preconditions

```sh
cd "/path/to/wscrpt"
git status                    # clean
git branch --show-current     # main
gh auth status                # must succeed
scripts/audit-public-source.sh
scripts/verify.sh             # or re-use a green pass from the same tip
cargo publish --dry-run --locked
```

## 1. Authenticate GitHub CLI

```sh
gh auth login -h github.com
# or refresh the token for account wcasse
gh auth status
```

## 2. Push main

```sh
git push origin main
```

## 3. Tag and push the tag

```sh
git tag -a v0.2.1 -m "wscrpt 0.2.1"
git push origin v0.2.1
```

If `v0.2.1` already exists remotely on a different commit: **stop**. Ship a new
patch version instead of moving the tag.

## 4. GitHub Releases

```sh
gh release create v0.2.0 \
  --title "wscrpt 0.2.0" \
  --notes-file docs/releases/v0.2.0.md \
  --verify-tag || true   # skip if already published

gh release create v0.2.1 \
  --title "wscrpt 0.2.1" \
  --notes-file docs/releases/v0.2.1.md \
  --verify-tag
```

## 5. Repo discoverability (one-time settings)

```sh
gh repo edit wcasse/wscrpt \
  --add-topic terminal \
  --add-topic editor \
  --add-topic ide \
  --add-topic ssh \
  --add-topic mosh \
  --add-topic ipad \
  --add-topic rust
```

Enable **private vulnerability reporting** in GitHub:
Repository → Settings → Code security → Private vulnerability reporting → Enable.
(`SECURITY.md` already points reporters there.)

## 6. crates.io

```sh
cargo login    # if needed
cargo publish --dry-run --locked
cargo publish --locked
```

## 7. Post-publish sanity

```sh
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.1 --locked --force
cd /tmp && wscrpt --version && wscrpt --health
```

Optional after crates.io indexes:

```sh
cargo install wscrpt --locked --force
```

## Abort conditions

- Dirty tree or unexpected untracked secrets
- `scripts/audit-public-source.sh` fails
- `scripts/verify.sh` fails
- `gh auth` invalid
- Tag `v0.2.1` already points at a different commit on origin
- Will did not authorize push
