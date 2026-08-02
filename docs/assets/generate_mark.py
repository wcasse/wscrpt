#!/usr/bin/env python3
"""Generate Esc-w mark assets, OG, and project banners for wscrpt."""
from PIL import Image, ImageDraw, ImageFont, ImageFilter
from pathlib import Path

OUT = Path(__file__).resolve().parent
BG = (0x0B, 0x0B, 0x0C)
FG = (0xF5, 0xF5, 0xF7)
INDIGO = (0x63, 0x66, 0xF1)  # #6366F1
MUTED = (0xA1, 0xA1, 0xA6)
BORDER = (0x2C, 0x2C, 0x2E)

ACCENT_HEX = "#6366F1"
TAGLINE_1 = "Terminal IDE for Mac and Linux —"
TAGLINE_2 = "iPad-first, any solid SSH client."
REPO = "github.com/wcasse/wscrpt"


def load_font(sz: int):
    for path in (
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/Library/Fonts/SF-Mono-Regular.otf",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
    ):
        try:
            return ImageFont.truetype(path, sz, index=0)
        except Exception:
            continue
    return ImageFont.load_default()


def draw_esc_w(size: int) -> Image.Image:
    im = Image.new("RGB", (size, size), BG)
    d = ImageDraw.Draw(im)
    s = size / 512.0
    stroke = max(2, int(28 * s))
    pts = [(150 * s, 176 * s), (108 * s, 256 * s), (150 * s, 336 * s)]
    d.line(pts, fill=INDIGO, width=stroke, joint="miter")
    font_size = int(200 * s)
    font = load_font(font_size)
    text = "w"
    cx, cy = 300 * s, 290 * s
    bbox = d.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    d.text((cx - tw / 2 - bbox[0], cy - th / 2 - bbox[1]), text, font=font, fill=FG)
    return im


def _soft_glow(w: int, h: int) -> Image.Image:
    """Subtle indigo radial wash for banners (not neon spam)."""
    layer = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    # soft ellipse upper-right
    cx, cy = int(w * 0.78), int(h * 0.35)
    rx, ry = int(w * 0.45), int(h * 0.70)
    for i, alpha in enumerate((28, 18, 10, 5)):
        pad = i * 40
        d.ellipse(
            [cx - rx - pad, cy - ry - pad, cx + rx + pad, cy + ry + pad],
            fill=(INDIGO[0], INDIGO[1], INDIGO[2], alpha),
        )
    layer = layer.filter(ImageFilter.GaussianBlur(radius=max(24, w // 40)))
    base = Image.new("RGBA", (w, h), (*BG, 255))
    return Image.alpha_composite(base, layer).convert("RGB")


def compose_banner(width: int, height: int, *, show_url: bool = True) -> Image.Image:
    """Horizontal project banner: mark + wordmark + tagline."""
    im = _soft_glow(width, height)
    d = ImageDraw.Draw(im)

    # Layout scale from height
    mark_size = int(height * 0.52)
    mark_size = max(140, min(mark_size, 320))
    margin_x = int(width * 0.055)
    mark = draw_esc_w(mark_size)
    mark_y = (height - mark_size) // 2
    im.paste(mark, (margin_x, mark_y))

    text_x = margin_x + mark_size + int(width * 0.035)
    # Type sizes relative to height
    title_sz = max(42, int(height * 0.14))
    tag_sz = max(20, int(height * 0.055))
    url_sz = max(16, int(height * 0.042))
    title_f = load_font(title_sz)
    tag_f = load_font(tag_sz)
    url_f = load_font(url_sz)

    # Vertical stack centered against mark
    lines = ["wscrpt", TAGLINE_1, TAGLINE_2]
    if show_url:
        lines.append(REPO)

    # Measure block height
    line_gaps = [int(title_sz * 0.35), int(tag_sz * 0.55), int(tag_sz * 0.35), int(url_sz * 0.9)]
    heights = []
    fonts = [title_f, tag_f, tag_f, url_f]
    colors = [FG, FG, INDIGO, MUTED]
    for i, line in enumerate(lines):
        bb = d.textbbox((0, 0), line, font=fonts[i])
        heights.append(bb[3] - bb[1])

    total_h = sum(heights) + sum(line_gaps[: len(lines) - 1])
    # Accent bar above title
    bar_h = max(4, height // 80)
    bar_w = max(48, int(title_sz * 1.1))
    y0 = (height - total_h - bar_h - int(title_sz * 0.35)) // 2

    d.rectangle([text_x, y0, text_x + bar_w, y0 + bar_h], fill=INDIGO)
    y = y0 + bar_h + int(title_sz * 0.35)

    for i, line in enumerate(lines):
        d.text((text_x, y), line, font=fonts[i], fill=colors[i])
        y += heights[i]
        if i < len(lines) - 1:
            y += line_gaps[i]

    # Bottom hairline accent
    d.rectangle([0, height - 3, width, height], fill=INDIGO)

    # Optional subtle left edge
    d.rectangle([0, 0, 3, height], fill=INDIGO)

    return im


def main():
    svg = f"""<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" role="img" aria-label="wscrpt">
  <rect width="512" height="512" fill="#0B0B0C"/>
  <path d="M150 176 L108 256 L150 336" fill="none" stroke="{ACCENT_HEX}"
        stroke-width="28" stroke-linecap="square" stroke-linejoin="miter"/>
  <text x="300" y="340" text-anchor="middle"
        font-family="ui-monospace, SFMono-Regular, Menlo, monospace"
        font-size="200" font-weight="500" fill="#F5F5F7">w</text>
</svg>
"""
    (OUT / "mark.svg").write_text(svg, encoding="utf-8")
    draw_esc_w(512).save(OUT / "mark-512.png")
    draw_esc_w(32).save(OUT / "mark-32.png")
    draw_esc_w(16).save(OUT / "mark-16.png")
    draw_esc_w(400).save(OUT / "avatar-400.png")

    # Project banners
    compose_banner(1280, 640).save(OUT / "banner-1280x640.png", optimize=True)
    compose_banner(1500, 500).save(OUT / "banner-1500x500.png", optimize=True)
    # Canonical alias used in README / GitHub social
    compose_banner(1280, 640).save(OUT / "banner.png", optimize=True)

    # OG / link unfurl (same family as banner, classic 1.91:1)
    compose_banner(1200, 630).save(OUT / "og-1200x630.png", optimize=True)

    print("wrote marks + banners + og into", OUT, f"(accent {ACCENT_HEX})")
    for name in (
        "banner.png",
        "banner-1280x640.png",
        "banner-1500x500.png",
        "og-1200x630.png",
    ):
        p = OUT / name
        print(f"  {name}: {p.stat().st_size // 1024}KB")


if __name__ == "__main__":
    main()
