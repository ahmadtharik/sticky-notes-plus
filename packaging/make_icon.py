"""Draws the app icon. Rerun only to change it: needs Pillow, outputs are committed."""
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SCALE = 8
PAPER = (255, 214, 64, 255)
FOLD = (224, 170, 28, 255)
LINE = (122, 88, 12, 255)


def draw(size: int) -> Image.Image:
    full = size * SCALE
    image = Image.new("RGBA", (full, full), (0, 0, 0, 0))
    pen = ImageDraw.Draw(image)
    inset = round(full * 0.06)
    right = full - inset
    fold = round(full * 0.30)
    radius = round(full * 0.10)
    pen.rounded_rectangle((inset, inset, right, right), radius, fill=PAPER)
    # Cut the bottom-right corner away, then lay the turned-up flap over it.
    pen.polygon([(right - fold, right + 1), (right + 1, right - fold), (right + 1, right + 1)], fill=(0, 0, 0, 0))
    pen.polygon([(right - fold, right), (right, right - fold), (right - fold, right - fold)], fill=FOLD)
    # Small sizes can only hold two lines of "writing".
    rows = (0.34, 0.54) if size < 32 else (0.28, 0.42, 0.56)
    thickness = max(round(full * 0.055), SCALE)
    left = round(full * 0.22)
    for index, row in enumerate(rows):
        end = full * (0.78 if index < len(rows) - 1 else 0.56)
        y = round(full * row)
        pen.rounded_rectangle((left, y, round(end), y + thickness), thickness // 2, fill=LINE)
    return image.resize((size, size), Image.LANCZOS)


sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
images = [draw(size) for size in sizes]
images[-1].save(ROOT / "packaging" / "app.ico", sizes=[(s, s) for s in sizes], append_images=images[:-1])
draw(256).save(ROOT / "ui" / "icons" / "app-icon.png")
draw(32).save(ROOT / "ui" / "icons" / "tray-icon.png")
