"""Generate the WinPods application icon files (run once, commit the output).

    python make_icons.py            # writes next to this script

Outputs:
  winpods.ico     16/24/32/48/256 px, embedded into WinPods.exe by build.rs
  winpods.png     256 px, the Slint window icon
  winpods-64.png  64 px, the logo in the window header and About card

Each size is drawn separately at 8x and downsampled, so the small sizes get a bolder,
simpler glyph instead of a blurry scaled-down copy. Requires Pillow.
"""

from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
ICO_SIZES = [16, 24, 32, 48, 256]
SUPERSAMPLE = 8

# Vertical gradient of the rounded tile (Windows blue).
TOP = (0x4C, 0x9C, 0xFF)
BOTTOM = (0x10, 0x5C, 0xD8)


def lerp(a: tuple[int, int, int], b: tuple[int, int, int], t: float) -> tuple[int, int, int, int]:
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b)) + (255,)


def draw_icon(size: int) -> Image.Image:
    s = size * SUPERSAMPLE
    small = size <= 24
    margin = s * (0.03 if small else 0.06)
    radius = s * 0.22

    # Rounded tile filled with the gradient.
    gradient = Image.new("RGBA", (s, s))
    gd = ImageDraw.Draw(gradient)
    for y in range(s):
        gd.line([(0, y), (s, y)], fill=lerp(TOP, BOTTOM, y / (s - 1)))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (margin, margin, s - 1 - margin, s - 1 - margin), radius=radius, fill=255
    )
    image = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    image.paste(gradient, (0, 0), mask)

    # White speaker glyph, in unit coordinates.
    d = ImageDraw.Draw(image)
    dx = 0.04 if small else 0.0

    def p(x: float, y: float) -> tuple[float, float]:
        return ((x + dx) * s, y * s)

    if small:
        d.rectangle([p(0.19, 0.38), p(0.34, 0.62)], fill="white")
        d.polygon([p(0.33, 0.38), p(0.52, 0.22), p(0.52, 0.78), p(0.33, 0.62)], fill="white")
    else:
        d.rectangle([p(0.23, 0.40), p(0.35, 0.60)], fill="white")
        d.polygon([p(0.34, 0.40), p(0.52, 0.25), p(0.52, 0.75), p(0.34, 0.60)], fill="white")

    waves = [(0.20, 0.13)] if small else [(0.15, 0.065), (0.27, 0.065)]
    cx, cy = p(0.50, 0.50)
    for r, w in waves:
        r_px, w_px = r * s, w * s
        box = [cx - r_px, cy - r_px, cx + r_px, cy + r_px]
        d.arc(box, start=-48, end=48, fill="white", width=round(w_px))
        # Round caps.
        for angle in (-48, 48):
            a = math.radians(angle)
            ex = cx + (r_px - w_px / 2) * math.cos(a)
            ey = cy + (r_px - w_px / 2) * math.sin(a)
            d.ellipse([ex - w_px / 2, ey - w_px / 2, ex + w_px / 2, ey + w_px / 2], fill="white")

    return image.resize((size, size), Image.LANCZOS)


def main() -> None:
    frames = {size: draw_icon(size) for size in ICO_SIZES}
    largest = frames[256]
    largest.save(
        HERE / "winpods.ico",
        format="ICO",
        sizes=[(n, n) for n in ICO_SIZES],
        append_images=[frames[n] for n in ICO_SIZES if n != 256],
    )
    largest.save(HERE / "winpods.png", optimize=True)
    draw_icon(64).save(HERE / "winpods-64.png", optimize=True)
    print("wrote", ", ".join(p.name for p in sorted(HERE.glob("winpods*"))))


if __name__ == "__main__":
    main()
