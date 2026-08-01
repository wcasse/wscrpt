# Publish runbook — v0.2.2 (SHIP lane)

Tag **HEAD** as `v0.2.2` after packaging commits land and gates pass.
Do **not** move an existing public tag once it has been published on that commit.

Identity: new commits must use GitHub noreply `163216174+wcasse@users.noreply.github.com`
(personal mailbox was scrubbed in an earlier rewrite; do not reintroduce it).

Repo ownership remains `wcasse`.

## Preconditions

```sh
cd /path/to/wscrpt
git status                    # clean
git branch --show-current     # main (or ship branch already merged)
gh auth status                # must succeed
scripts/audit-public-source.sh
scripts/audit-public-source.sh --history
scripts/verify.sh             # or re-use a green pass from the same tip
cargo publish --dry-run --locked
```

**History note:** if `--history` fails on author email for the two tip commits that
used a personal mailbox, fix with an owner-authorized email-only rewrite (or
soft rebase of those commits) **before** tagging. Prefer noreply for all future
commits (`git config user.email '163216174+wcasse@users.noreply.github.com'`).

## 1. Authenticate GitHub CLI

```sh
gh auth login -h github.com
gh auth status
```

## 2. Push main

```sh
git push origin main
```

## 3. Tag and push the tag

```sh
git tag -a v0.2.2 -m "wscrpt 0.2.2"
git push origin v0.2.2
```

If `v0.2.2` already exists remotely on a different commit: **stop**. Ship a new
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
  --verify-tag || true

gh release create v0.2.2 \
  --title "wscrpt 0.2.2" \
  --notes-file docs/releases/v0.2.2.md \
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
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.2 --locked --force
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
- Tag `v0.2.2` already points at a different commit on origin
- Will did not authorize push / tag / publish
