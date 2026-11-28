"""Draw the three program icons (listener ADR-085).

Run from the workspace root after changing a shape or a colour:

    python assets/icons/make_icons.py

It needs Pillow and writes, next to itself, an .ico per program (16, 24, 32,
48 and 256 px) and a 256 px .png for the window icon. Each shape is drawn
eight times larger than its target size and scaled down, for smooth edges.

The drawings sit on a 64-unit grid. From 32 px up they use the large
geometry; at 24 and 16 px the dot at the wire's end is left out and lines are
thicker, since fine detail only blurs at those sizes.
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SUPERSAMPLE = 8
ICO_SIZES = [16, 24, 32, 48, 256]
SMALL = 24  # this size and below use the small geometry


def rgb(hex_colour, alpha=255):
    value = int(hex_colour.lstrip("#"), 16)
    return (value >> 16 & 0xFF, value >> 8 & 0xFF, value & 0xFF, alpha)


class Pen:
    """Draws on the 64-unit grid at one scale."""

    def __init__(self, image):
        self.image = image
        self.draw = ImageDraw.Draw(image)
        self.k = image.width / 64

    def p(self, x, y):
        return (x * self.k, y * self.k)

    def tile(self, colour):
        k = self.k
        self.draw.rounded_rectangle(
            [2 * k, 2 * k, 62 * k, 62 * k], radius=14 * k, fill=rgb(colour)
        )

    def dot(self, x, y, r, colour):
        k = self.k
        self.draw.ellipse(
            [(x - r) * k, (y - r) * k, (x + r) * k, (y + r) * k], fill=colour
        )

    def polyline(self, points, width, colour):
        """Line segments with round caps and joins."""
        colour = rgb(colour)
        for a, b in zip(points, points[1:]):
            self.draw.line([self.p(*a), self.p(*b)], fill=colour, width=round(width * self.k))
        for x, y in points:
            self.dot(x, y, width / 2, colour)

    def triangle(self, points, round_by, colour):
        """A filled triangle with its corners rounded by a stroke."""
        self.draw.polygon([self.p(*q) for q in points], fill=rgb(colour))
        self.polyline(points + points[:1], round_by, colour)

    def ring(self, x, y, r, width, colour):
        k = self.k
        outer = r + width / 2
        self.draw.ellipse(
            [(x - outer) * k, (y - outer) * k, (x + outer) * k, (y + outer) * k],
            outline=rgb(colour),
            width=round(width * k),
        )

    def tint_disc(self, x, y, r, colour, alpha):
        layer = Image.new("RGBA", self.image.size, (0, 0, 0, 0))
        k = self.k
        ImageDraw.Draw(layer).ellipse(
            [(x - r) * k, (y - r) * k, (x + r) * k, (y + r) * k], fill=rgb(colour, alpha)
        )
        self.image.alpha_composite(layer)
        self.draw = ImageDraw.Draw(self.image)


def talker(pen, small):
    pen.tile("#0B2A5B")
    pen.polyline([(10, 32), (36, 32)], 7 if small else 6, "#5FA5FF")
    if not small:
        pen.dot(10, 32, 5, rgb("#5FA5FF"))
    pen.triangle([(34, 18), (54, 32), (34, 46)], 3, "#FFFFFF")


def listener(pen, small):
    pen.tile("#1E5BC6")
    pen.polyline([(24, 32), (54, 32)], 7 if small else 6, "#A9CDFF")
    if not small:
        pen.dot(54, 32, 5, rgb("#A9CDFF"))
    pen.polyline([(32, 17), (13, 17), (13, 47), (32, 47)], 9 if small else 8, "#FFFFFF")


def linemon(pen, small):
    pen.tile("#D3E3FF")
    pen.polyline([(8, 29), (56, 29)], 7 if small else 6, "#1E5BC6")
    pen.tint_disc(28, 29, 13, "#1E5BC6", round(0.2 * 255))
    pen.ring(28, 29, 13, 7.5 if small else 6, "#0B2A5B")
    pen.polyline([(38, 39), (51, 52)], 9 if small else 8, "#0B2A5B")


def render(draw_icon, size):
    big = Image.new("RGBA", (size * SUPERSAMPLE,) * 2, (0, 0, 0, 0))
    draw_icon(Pen(big), small=size <= SMALL)
    return big.resize((size, size), Image.LANCZOS)


def main():
    for name, draw_icon in [("talker", talker), ("listener", listener), ("linemon", linemon)]:
        images = {size: render(draw_icon, size) for size in ICO_SIZES}
        images[256].save(HERE / f"{name}.png")
        images[256].save(
            HERE / f"{name}.ico",
            sizes=[(size, size) for size in ICO_SIZES],
            append_images=[images[size] for size in ICO_SIZES if size != 256],
        )
        print(f"wrote {name}.png and {name}.ico")


if __name__ == "__main__":
    main()
