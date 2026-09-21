#!/usr/bin/env python3
"""Compose the RayTerm logo lockup: the family mark with the wordmark beneath it.

The mark is used as-is from `brand/rayterm-mark.png` — the same file every shipped icon is
derived from. It is *placed*, never redrawn: a lockup whose mark differs from the icon's mark
is two brands, not one, and a redraw is exactly how that happens.

The plate follows the family presentation: a light rounded square with a hairline inset
border, the mark centred above the wordmark.

Run from `oxideterm/`:
    python3 scripts/branding/generate_rayterm_logo.py

The output is committed because the logo is a branding asset rather than a build artifact.
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# Imported rather than copied: the accent is defined once, and a logo that recoloured the mark
# differently from the icons would be a second brand. The icon script is the source of truth
# for both the colour and the mapping.
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from generate_rayterm_icons import RAYTERM_ACCENT, recolor  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent.parent
MARK = ROOT / "crates/oxideterm-gpui-app/resources/icons/brand/rayterm-mark.png"
WORDMARK_FONT = ROOT / "assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf"
OUTPUT = ROOT / "crates/oxideterm-gpui-app/resources/icons/brand/rayterm-logo-master.png"

SIZE = 1254
# The plate's inset from the canvas edge, leaving the margin the family master has.
PLATE_MARGIN = 24
CORNER_RADIUS = 180
BORDER_INSET = 16

WORDMARK = "RayTerm"
# Faux bold: the bundled family ships Regular only, and a stroke keeps the project typeface
# rather than substituting a system face that would not match the rest of the product.
WORDMARK_STROKE = 3

# The same ink the icon generator maps the mark's near-black to, so the wordmark and the mark
# cannot drift apart.
INK = (24, 26, 27)
BORDER = (214, 214, 214)
PLATE_TOP = (250, 250, 249)
PLATE_BOTTOM = (241, 241, 240)


def plate(size: int) -> Image.Image:
    """The light rounded-square background, with the hairline border the family master has."""
    background = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    inner = size - 2 * PLATE_MARGIN
    # A vertical gradient rather than a flat fill: the family master is not flat, and a flat
    # plate next to it reads as a different, cheaper asset.
    gradient = Image.new("RGBA", (inner, inner))
    pixels = gradient.load()
    for y in range(inner):
        blend = y / max(inner - 1, 1)
        row = tuple(
            round(PLATE_TOP[channel] + (PLATE_BOTTOM[channel] - PLATE_TOP[channel]) * blend)
            for channel in range(3)
        )
        for x in range(inner):
            pixels[x, y] = (*row, 255)

    mask = Image.new("L", (inner, inner), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, inner - 1, inner - 1), radius=CORNER_RADIUS, fill=255
    )
    background.paste(gradient, (PLATE_MARGIN, PLATE_MARGIN), mask)

    draw = ImageDraw.Draw(background)
    draw.rounded_rectangle(
        (
            PLATE_MARGIN + BORDER_INSET,
            PLATE_MARGIN + BORDER_INSET,
            size - PLATE_MARGIN - BORDER_INSET - 1,
            size - PLATE_MARGIN - BORDER_INSET - 1,
        ),
        radius=CORNER_RADIUS - BORDER_INSET,
        outline=BORDER,
        width=2,
    )
    return background


def main() -> None:
    canvas = plate(SIZE)
    # Recoloured exactly as the icons are, so the logo and the app icon are the same mark in the
    # same colours rather than two near-misses.
    mark = recolor(Image.open(MARK).convert("RGBA"), INK, RAYTERM_ACCENT)

    # Sized against the plate rather than in absolute pixels, so changing SIZE keeps the
    # proportions the family master has.
    mark_height = round(SIZE * 0.34)
    mark_width = round(mark.width * mark_height / mark.height)
    mark = mark.resize((mark_width, mark_height), Image.LANCZOS)
    mark_top = round(SIZE * 0.17)
    canvas.alpha_composite(mark, ((SIZE - mark_width) // 2, mark_top))

    font = ImageFont.truetype(str(WORDMARK_FONT), round(SIZE * 0.105))
    draw = ImageDraw.Draw(canvas)
    box = draw.textbbox((0, 0), WORDMARK, font=font, stroke_width=WORDMARK_STROKE)
    text_width = box[2] - box[0]
    text_height = box[3] - box[1]
    text_top = mark_top + mark_height + round(SIZE * 0.05)
    draw.text(
        ((SIZE - text_width) // 2 - box[0], text_top - box[1]),
        WORDMARK,
        font=font,
        fill=INK,
        stroke_width=WORDMARK_STROKE,
        stroke_fill=INK,
    )

    canvas.save(OUTPUT)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({SIZE}x{SIZE})")
    print(f"  mark   {mark_width}x{mark_height} at y={mark_top}")
    print(f"  word   {text_width}x{text_height} at y={text_top}")


if __name__ == "__main__":
    main()
