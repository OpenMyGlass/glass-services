#!/usr/bin/env python3
"""Printable A4 camera test chart for the Model B camera comparison.

Usage: make-test-chart.py OUT.pdf

Vector PDF plus nominal values in OUT.json. A home print is not a colour
reference: use it to compare two cameras on the same sheet in the same light.
"""
import json
import math
import subprocess
import sys

import cairo

MM = 72 / 25.4
W, H = 297, 210

# Nominal sRGB, in the usual order of 24-patch charts.
PATCHES = [
    (115, 82, 68), (194, 150, 130), (98, 122, 157), (87, 108, 67), (133, 128, 177), (103, 189, 170),
    (214, 126, 44), (80, 91, 166), (193, 90, 99), (94, 60, 108), (157, 188, 64), (224, 163, 46),
    (56, 61, 150), (70, 148, 73), (175, 54, 60), (231, 199, 31), (187, 86, 149), (8, 133, 161),
    (243, 243, 242), (200, 200, 200), (160, 160, 160), (122, 122, 121), (85, 85, 85), (52, 52, 52),
]
QR_SHORT = "OpenMyGlass test chart"
# About the length of a Wi-Fi setup or pairing offer. The values are made up.
QR_LONG = "OMG-TEST:" + "0123456789abcdef" * 7


def rgb(c, v):
    c.set_source_rgb(v[0] / 255, v[1] / 255, v[2] / 255)


def rect(c, x, y, w, h, v):
    rgb(c, v)
    c.rectangle(x, y, w, h)
    c.fill()


def text(c, x, y, s, size=3.2, bold=False):
    c.set_source_rgb(0, 0, 0)
    c.select_font_face("sans-serif", cairo.FONT_SLANT_NORMAL,
                       cairo.FONT_WEIGHT_BOLD if bold else cairo.FONT_WEIGHT_NORMAL)
    c.set_font_size(size)
    c.move_to(x, y)
    c.show_text(s)


def qr_modules(payload):
    out = subprocess.run(["qrencode", "-t", "ASCII", "-m", "0", "-l", "M", "--", payload],
                         check=True, capture_output=True, text=True).stdout
    return [[row[i] == "#" for i in range(0, len(row), 2)] for row in out.splitlines() if row]


def qr(c, x, y, size, payload):
    m = qr_modules(payload)
    n = len(m)
    d = size / n
    # White quiet zone of 4 modules.
    rect(c, x - 4 * d, y - 4 * d, size + 8 * d, size + 8 * d, (255, 255, 255))
    c.set_source_rgb(0, 0, 0)
    for j, row in enumerate(m):
        for i, on in enumerate(row):
            if on:
                c.rectangle(x + i * d, y + j * d, d, d)
    c.fill()
    return n


def star(c, cx, cy, r, spokes=72):
    c.set_source_rgb(0, 0, 0)
    step = 2 * math.pi / spokes
    for k in range(0, spokes, 2):
        c.move_to(cx, cy)
        c.arc(cx, cy, r, k * step, (k + 1) * step)
        c.close_path()
    c.fill()


def slanted(c, cx, cy, side, deg=5.0):
    rect(c, cx - side * 0.8, cy - side * 0.8, side * 1.6, side * 1.6, (205, 205, 205))
    c.save()
    c.translate(cx, cy)
    c.rotate(math.radians(deg))
    rect(c, -side / 2, -side / 2, side, side, (50, 50, 50))
    c.restore()


def fiducial(c, x, y):
    rect(c, x, y, 10, 10, (0, 0, 0))
    rect(c, x + 3, y + 3, 4, 4, (255, 255, 255))


def ruler(c, x, y, length=100):
    c.set_source_rgb(0, 0, 0)
    c.set_line_width(0.25)
    c.move_to(x, y)
    c.line_to(x + length, y)
    for mm in range(0, length + 1, 10):
        c.move_to(x + mm, y)
        c.line_to(x + mm, y - (4 if mm % 50 == 0 else 2))
    c.stroke()
    text(c, x, y + 4, f"{length} mm: misura con un righello. Se non torna, ristampa al 100%", 2.8)


def main(path):
    surface = cairo.PDFSurface(path, W * MM, H * MM)
    surface.set_metadata(cairo.PDF_METADATA_TITLE, "OpenMyGlass camera test chart")
    c = cairo.Context(surface)
    c.scale(MM, MM)
    rect(c, 0, 0, W, H, (255, 255, 255))

    for x, y in ((8, 8), (W - 18, 8), (8, H - 18), (W - 18, H - 18)):
        fiducial(c, x, y)
    text(c, 24, 14, "OpenMyGlass, tabella di prova fotocamera v1", 4.5, True)

    for i, v in enumerate(PATCHES):
        rect(c, 24 + (i % 6) * 22, 20 + (i // 6) * 22, 20, 20, v)

    steps = [round(255 * i / 10) for i in range(11)]
    for i, g in enumerate(steps):
        rect(c, 24 + i * 12, 110, 12, 14, (g, g, g))
    c.set_source_rgb(0, 0, 0)
    c.set_line_width(0.15)
    c.rectangle(24, 110, 132, 14)
    c.stroke()

    star(c, 192, 52, 30)
    slanted(c, 252, 52, 32)
    slanted(c, 192, 110, 18)
    slanted(c, 252, 110, 18, -5.0)

    short_n = qr(c, 28, 134, 30, QR_SHORT)
    long_n = qr(c, 74, 134, 50, QR_LONG)
    qr(c, 140, 134, 15, QR_SHORT)
    text(c, 28, 169, "breve, 30 mm", 2.8)
    text(c, 74, 189, "lungo, 50 mm", 2.8)
    text(c, 140, 154, "breve, 15 mm", 2.8)
    ruler(c, 170, 176)
    text(c, 170, 188, "Stampa al 100%, carta opaca, senza adattare alla pagina", 2.8)
    surface.finish()

    with open(path.rsplit(".", 1)[0] + ".json", "w") as f:
        json.dump({"patches_srgb": PATCHES, "grey_steps": steps, "slant_deg": 5.0,
                   "qr": {"short": QR_SHORT, "long": QR_LONG,
                          "short_modules": short_n, "long_modules": long_n}}, f, indent=1)


if __name__ == "__main__":
    main(sys.argv[1])
