#!/usr/bin/env python3
"""Build the RayTerm application icons from the Ray family mark.

The mark is checked in as `brand/rayterm-mark.png`: the R from the family design sheet,
already separated from the wordmark and converted to transparency. Deriving every icon
from that one file keeps the variants consistent, and a change of accent colour or plate
is then a diff here rather than a re-export from a design tool.

Run from the repository root:

    python3 scripts/branding/generate_rayterm_icons.py

The generated files are committed because the application embeds them; this script is for
changing the branding rather than for the build.

Composition: the family lockup puts the mark flush with the left of its box and leaves the
right side empty, so the icon reads as a squared block with the accent inside it. The mark
as extracted has a rounded right edge and a sloping lower edge, which is right for the
wordmark but leaves a bare plate looking lopsided. `squared_mask` rectifies the mark's left
portion to the full height while keeping the bowl's curve.
"""

from __future__ import annotations

import argparse
import math
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw

# RayTerm takes the teal from the application's existing accent set; it is distinct from
# the four the family pairs with its other products (lime, blue, orange, cyan).
RAYTERM_ACCENT = (20, 160, 168)
INK = (24, 26, 27)
PAPER = (255, 255, 255)
CHARCOAL = (32, 38, 46)

MARK_MARGIN = 0.18  # the mark's inset from the plate edge; enough that the bowl clears the rounded corner
PLATE_RADIUS = 0.22
SUPERSAMPLE = 4

ICON_SIZE = 512
ICNS_SIZES = [16, 32, 64, 128, 256, 512, 1024]
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# The Windows Store tile assets. They live beside `icon.png`, are referenced only by the
# packaging configuration, and were still the pre-rebrand mark; regenerating them from the
# same master keeps every shipped icon on one design.
STORE_LOGOS = {
    "StoreLogo.png": 50,
    "Square30x30Logo.png": 30,
    "Square44x44Logo.png": 44,
    "Square71x71Logo.png": 71,
    "Square89x89Logo.png": 89,
    "Square107x107Logo.png": 107,
    "Square142x142Logo.png": 142,
    "Square150x150Logo.png": 150,
    "Square284x284Logo.png": 284,
    "Square310x310Logo.png": 310,
}


def load_mark(brand_dir: Path) -> Image.Image:
    path = brand_dir / "rayterm-mark.png"
    if not path.is_file():
        raise SystemExit(f"the master mark is missing: {path}")
    return Image.open(path).convert("RGBA")


def recolor(mark, ink, accent):
    """Maps the source mark's lime to `accent` and its near-black to `ink`.

    Matching is by hue rather than exact value, because the source is a photograph of a
    printed sheet: its lime spans a range along the anti-aliased edges.
    """
    mark = mark.copy()
    pixels = mark.load()
    width, height = mark.size
    ink_r, ink_g, ink_b = ink
    acc_r, acc_g, acc_b = accent

    for y in range(height):
        for x in range(width):
            r, g, b, a = pixels[x, y]
            if a == 0:
                continue
            is_accent = g > 120 and g - b > 40 and r < 235
            pixels[x, y] = (acc_r, acc_g, acc_b, a) if is_accent else (ink_r, ink_g, ink_b, a)
    return mark


def compose(size, mark, background, ink, accent):
    """Draws one icon: the plate, then the recoloured mark centred on it."""
    scale = size * SUPERSAMPLE
    canvas = Image.new("RGBA", (scale, scale), (0, 0, 0, 0))

    if background is not None:
        ImageDraw.Draw(canvas).rounded_rectangle(
            [(0, 0), (scale - 1, scale - 1)],
            radius=int(scale * PLATE_RADIUS),
            fill=(*background, 255),
        )

    # The mark keeps its own silhouette — its rounded bowl and diagonal leg *are* the
    # letter, so rectifying them to a square destroys the shape. It is fitted inside the
    # plate's inner box on both axes and centred, so a mark wider than its height cannot
    # overflow the plate.
    inner = scale * (1 - 2 * MARK_MARGIN)
    ratio = mark.width / mark.height
    if ratio >= 1.0:
        target_w, target_h = int(inner), max(1, int(inner / ratio))
    else:
        target_h, target_w = int(inner), max(1, int(inner * ratio))

    colored = recolor(mark.resize((target_w, target_h), Image.LANCZOS), ink, accent)
    canvas.alpha_composite(
        colored, ((scale - target_w) // 2, (scale - target_h) // 2)
    )
    return canvas.resize((size, size), Image.LANCZOS)


# The application already ships a variant picker under these names; the files are
# regenerated in place so a stored user setting keeps resolving.
VARIANTS = {
    "default": (PAPER, INK, RAYTERM_ACCENT),
    "white-blue": (PAPER, INK, (55, 142, 246)),
    "white-graphite": (PAPER, INK, (106, 116, 135)),
    "white-green": (PAPER, INK, (31, 161, 73)),
    "white-purple": (PAPER, INK, (129, 61, 229)),
    "white-red": (PAPER, INK, (221, 42, 38)),
    "filled-orange": (CHARCOAL, PAPER, RAYTERM_ACCENT),
    "filled-blue": (CHARCOAL, PAPER, (55, 142, 246)),
    "filled-graphite": (CHARCOAL, PAPER, (106, 116, 135)),
    "filled-green": (CHARCOAL, PAPER, (31, 161, 73)),
    "filled-purple": (CHARCOAL, PAPER, (129, 61, 229)),
    "filled-red": (CHARCOAL, PAPER, (221, 42, 38)),
}


def verify(path: Path, size: int, expect_accent) -> None:
    """Asserts the icon contains the mark and its accent.

    A mis-sized composite or a fully transparent mask still writes a valid PNG, so this
    counts pixels rather than trusting the file to exist.
    """
    image = Image.open(path).convert("RGBA")
    assert image.size == (size, size), f"{path} is {image.size}, expected {size}"

    opaque = [p for p in image.getdata() if p[3] > 200]
    assert len(opaque) > size * size * 0.3, f"{path} looks empty"
    assert [p for p in opaque if math.dist(p[:3], expect_accent) < 40], (
        f"{path} has no pixels of the accent colour {expect_accent}"
    )
    left = [p for p in image.crop((0, 0, size // 3, size)).getdata() if p[3] > 200]
    assert left, f"{path} has nothing in the left third, so the mark did not land"


def write_icns(out, mark, background, ink, accent) -> bool:
    iconset = out / "rayterm.iconset"
    if iconset.exists():
        for child in iconset.iterdir():
            child.unlink()
    iconset.mkdir(parents=True, exist_ok=True)

    for size in ICNS_SIZES:
        image = compose(size, mark, background, ink, accent)
        if size <= 512:
            image.save(iconset / f"icon_{size}x{size}.png", "PNG")
        if size >= 32:
            half = size // 2
            image.save(iconset / f"icon_{half}x{half}@2x.png", "PNG")

    target = out / "icon.icns"
    try:
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(target)],
            check=True,
            capture_output=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as error:
        print(f"  iconutil unavailable ({error}); icon.icns not regenerated")
        return False
    finally:
        for child in iconset.iterdir():
            child.unlink()
        iconset.rmdir()
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description="generate RayTerm application icons")
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("crates/oxideterm-gpui-app/resources/icons"),
        help="icon resource directory",
    )
    args = parser.parse_args()
    out: Path = args.out

    mark = load_mark(out / "brand")
    print(f"mark {mark.size}")
    print(f"writing icons to {out}")

    for name, (background, ink, accent) in VARIANTS.items():
        image = compose(ICON_SIZE, mark, background, ink, accent)
        png = out / "variants" / f"{name}.png"
        png.parent.mkdir(parents=True, exist_ok=True)
        image.save(png, "PNG", optimize=True)
        verify(png, ICON_SIZE, accent)
        image.save(out / "variants" / f"{name}.ico", "ICO", sizes=[(s, s) for s in ICO_SIZES])
        print(f"  {name}")

    background, ink, accent = VARIANTS["default"]
    icon = compose(ICON_SIZE, mark, background, ink, accent)
    icon.save(out / "icon.png", "PNG", optimize=True)
    verify(out / "icon.png", ICON_SIZE, accent)
    print("  icon.png")

    for name, store_size in STORE_LOGOS.items():
        store_icon = compose(store_size, mark, background, ink, accent)
        store_icon.save(out / name, "PNG", optimize=True)

    if write_icns(out, mark, background, ink, accent):
        print(f"  icon.icns ({(out / 'icon.icns').stat().st_size} bytes)")
    print(f"  {len(STORE_LOGOS)} Windows Store tiles")
    return 0


if __name__ == "__main__":
    sys.exit(main())
