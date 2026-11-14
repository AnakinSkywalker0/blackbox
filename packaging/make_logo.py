"""Draws the blackbox logo as PNG and a multi-size Windows .ico, matching docs/img/logo.svg.

Run from the repository root:  python packaging/make_logo.py
"""
from PIL import Image, ImageDraw

ORANGE = (255, 107, 26, 255)
INK = (22, 22, 22, 255)
# The heartbeat line, on the SVG's 64-unit grid.
LINE = [(11, 35), (22, 35), (27, 22), (34, 46), (39, 29), (43, 35), (53, 35)]


def draw(size: int) -> Image.Image:
    # Drawn 8x larger and scaled down, for smooth edges.
    big = size * 8
    k = big / 64
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([2 * k, 2 * k, 62 * k, 62 * k], radius=15 * k, fill=ORANGE)
    pts = [(x * k, y * k) for x, y in LINE]
    w = int(5 * k)
    d.line(pts, fill=INK, width=w, joint="curve")
    r = w / 2
    for x, y in (pts[0], pts[-1]):
        d.ellipse([x - r, y - r, x + r, y + r], fill=INK)
    return img.resize((size, size), Image.LANCZOS)


if __name__ == "__main__":
    draw(256).save("packaging/logo-256.png")
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    draw(256).save("packaging/blackbox.ico", sizes=[(s, s) for s in sizes])
    print("wrote packaging/logo-256.png and packaging/blackbox.ico")
