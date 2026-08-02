#!/usr/bin/env python3
"""Generate Esc-w mark assets and OG image for wscrpt launch."""
from PIL import Image, ImageDraw, ImageFont
from pathlib import Path

OUT = Path(__file__).resolve().parent
BG = (0x0B, 0x0B, 0x0C)
FG = (0xF5, 0xF5, 0xF7)
# Primary brand accent — indigo (not product ANSI cyan)
INDIGO = (0x63, 0x66, 0xF1)  # #6366F1
MUTED = (0xA1, 0xA1, 0xA6)

ACCENT_HEX = "#6366F1"


def draw_esc_w(size: int) -> Image.Image:
    im = Image.new("RGB", (size, size), BG)
    d = ImageDraw.Draw(im)
    s = size / 512.0
    stroke = max(2, int(28 * s))
    pts = [(150 * s, 176 * s), (108 * s, 256 * s), (150 * s, 336 * s)]
    d.line(pts, fill=INDIGO, width=stroke, joint="miter")
    font_size = int(200 * s)
    font = None
    for path in (
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/Library/Fonts/SF-Mono-Regular.otf",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
    ):
        try:
            font = ImageFont.truetype(path, font_size, index=0)
            break
        except Exception:
            continue
    if font is None:
        font = ImageFont.load_default()
    text = "w"
    cx, cy = 300 * s, 290 * s
    bbox = d.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    d.text((cx - tw / 2 - bbox[0], cy - th / 2 - bbox[1]), text, font=font, fill=FG)
    return im


def load_font(sz):
    for path in (
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
    ):
        try:
            return ImageFont.truetype(path, sz, index=0)
        except Exception:
            continue
    return ImageFont.load_default()


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
    og = Image.new("RGB", (1200, 630), BG)
    d = ImageDraw.Draw(og)
    og.paste(draw_esc_w(280), (80, (630 - 280) // 2))
    title_f, tag_f, url_f = load_font(72), load_font(28), load_font(22)
    x0 = 420
    d.text((x0, 200), "wscrpt", font=title_f, fill=FG)
    d.text((x0, 290), "Terminal IDE for real hosts —", font=tag_f, fill=FG)
    d.text((x0, 330), "iPad-first, any solid SSH client.", font=tag_f, fill=INDIGO)
    d.text((x0, 420), "github.com/wcasse/wscrpt", font=url_f, fill=MUTED)
    d.rectangle([x0, 180, x0 + 80, 186], fill=INDIGO)
    og.save(OUT / "og-1200x630.png")
    print("wrote marks + og into", OUT, f"(accent {ACCENT_HEX})")


if __name__ == "__main__":
    main()
