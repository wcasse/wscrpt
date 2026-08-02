# wscrpt brand assets

Install pin for public claims and demos: **`v0.2.2`** (resolve with `git rev-parse v0.2.2`).

## Re-record policy

- Record **`demo.gif` from the install-pin binary only** — never dirty `main` / tip.
- Preferred:

```sh
PIN=v0.2.2
git worktree add /tmp/wscrpt-$PIN $PIN
cd /tmp/wscrpt-$PIN && cargo build --release
# use charmbracelet vhs with docs/demo.tape (Output docs/assets/demo.gif)
# OR path the pin binary into PATH and run vhs from the main repo checkout
```

- Assert before recording: `wscrpt --version` matches the pin tag.
- Do **not** demo tip-only chords (e.g. sticky checklist `Esc w C` / `Esc w Y` while pin is v0.2.2).
- Assets are **git-tracked** but **excluded from the crates.io package** via `Cargo.toml` `exclude`.

## Export marks

Esc-w: indigo (`#6366F1`) action chevron + mono `w` on `#0B0B0C`.

```sh
# Regenerated with docs/assets/generate_mark.py (Pillow)
python3 docs/assets/generate_mark.py
```

| File | Use |
| --- | --- |
| `mark.svg` | Vector source |
| `mark-32.png` | Favicon-ish / small |
| `avatar-400.png` | Social avatar |
| `og-1200x630.png` | GitHub / link unfurl |
| `demo.gif` | README motion proof |

After commit: GitHub → Settings → General → Social preview → upload `og-1200x630.png` if the UI exposes it.

## Project banner

| File | Size | Use |
| --- | --- | --- |
| `banner.png` | 1280×640 | Canonical project banner (README / GH social) |
| `banner-1280x640.png` | 1280×640 | Same as `banner.png` |
| `banner-1500x500.png` | 1500×500 | Wide / X header style |
| `og-1200x630.png` | 1200×630 | Link unfurl / Open Graph |

Regenerate with `python3 docs/assets/generate_mark.py`.

After push: GitHub → **Settings → General → Social preview** → upload `banner.png` or `og-1200x630.png`.
