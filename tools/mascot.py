#!/usr/bin/env python3
"""Draw the Remote-Code mascot as braille art.

Braille cells are a 2x4 dot grid, so 18 cells across is 36 dots — pixel-art
resolution in a terminal. The welcome screen draws the art one glyph at a time
(it animates a shine across it), so any monochrome shape works; what matters is
that the silhouette reads as a cat at 7 rows and still at 5.

The shape is placed on exact dot coordinates and rendered as *line art*: on a dark
terminal a filled black cat would be an invisible blob, so the ink draws the
outline, the eyes and the whiskers.

Usage:  python3 tools/mascot.py            # both tiers, braille
        python3 tools/mascot.py --dots     # the dot view, to place features
"""

import sys

DOTS = [(0, 0, 1), (0, 1, 2), (0, 2, 4), (0, 3, 64), (1, 0, 8), (1, 1, 16), (1, 2, 32), (1, 3, 128)]

W, H = 36, 28


class Grid:
    def __init__(self, w=W, h=H):
        self.w, self.h = w, h
        self.dots = [[False] * w for _ in range(h)]

    def set(self, x, y):
        if 0 <= x < self.w and 0 <= y < self.h:
            self.dots[y][x] = True

    def ring(self, cx, cy, rx, ry, thickness=1.0):
        """An ellipse outline drawn as a distance band."""
        for y in range(self.h):
            for x in range(self.w):
                d = ((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2
                if 1.0 - thickness / min(rx, ry) < d <= 1.0:
                    self.set(x, y)

    def disc(self, cx, cy, rx, ry):
        cx, cy = float(cx), float(cy)
        for y in range(self.h):
            for x in range(self.w):
                if ((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2 <= 1.0:
                    self.set(x, y)

    def stroke(self, x0, y0, x1, y1, width=1):
        x0, y0, x1, y1 = (int(round(v)) for v in (x0, y0, x1, y1))
        steps = max(abs(x1 - x0), abs(y1 - y0), 1)
        for i in range(steps + 1):
            x = x0 + (x1 - x0) * i / steps
            y = y0 + (y1 - y0) * i / steps
            for dx in range(width):
                for dy in range(width):
                    self.set(int(round(x)) + dx, int(round(y)) + dy)

    def triangle(self, apex, base_a, base_b, thickness=2):
        (ax, ay), (bx, by), (cx, cy) = apex, base_a, base_b
        ay = int(round(ay))
        bottom = int(round(max(by, cy)))
        for y in range(ay, bottom + 1):
            t = (y - ay) / max(1, bottom - ay)
            x0 = ax + (bx - ax) * t
            x1 = ax + (cx - ax) * t
            for x in range(int(min(x0, x1)), int(max(x0, x1)) + 1):
                edge = x <= int(min(x0, x1)) + thickness - 1 or x >= int(max(x0, x1)) - thickness + 1
                if edge or thickness >= 2:
                    self.set(x, y)

    def dots_view(self):
        return "\n".join("".join("#" if d else "." for d in row) for row in self.dots)

    def braille(self):
        out = []
        for cell_y in range(self.h // 4):
            line = []
            for cell_x in range(self.w // 2):
                bits = 0
                for dx, dy, bit in DOTS:
                    if self.dots[cell_y * 4 + dy][cell_x * 2 + dx]:
                        bits |= bit
                line.append("⠀" if bits == 0 else chr(0x2800 + bits))
            line = "".join(line).rstrip()
            if line:
                out.append(line)
        return "\n".join(out)


def cat(head_cy=14.0, head_r=10.5, eye_cy=12.0, eye_r=2.4):
    """A kitten, front-on: ears, big eyes, nose, whiskers, two front paws.

    Line art on purpose: on a dark terminal a filled black cat is an invisible
    blob, so the ink draws the outline, the eyes and the whiskers, and the eyes
    are the only filled areas — they are what makes it read as a cat at a glance.
    """
    g = Grid()
    cx = 17.5
    crown = head_cy - head_r * 0.95
    # Head: a ring, thick enough to survive the shimmer gradient.
    g.ring(cx, head_cy, head_r, head_r * 0.95, thickness=1.4)
    # Ears: two narrow triangles sitting on the crown, a gap between them.
    g.triangle((11.0, 1.0), (8.0, crown), (15.0, crown), thickness=1)
    g.triangle((24.0, 1.0), (20.0, crown), (27.0, crown), thickness=1)
    # Eyes: filled, the one feature that reads at any size.
    g.disc(cx - 5.5, eye_cy, eye_r, eye_r)
    g.disc(cx + 5.5, eye_cy, eye_r, eye_r)
    # Nose: a small triangle, and a short mouth under it.
    nose_y = int(eye_cy + eye_r + 3)
    g.stroke(cx - 1, nose_y, cx + 1, nose_y)
    g.stroke(cx, nose_y + 1, cx, nose_y + 1)
    # Whiskers: three short dashes each side, clear of the ring.
    for step, y in enumerate((nose_y - 3, nose_y, nose_y + 3)):
        g.set(1 + step, y + 1)
        g.set(2 + step, y + 1)
        g.set(3 + step, y)
        g.set(4 + step, y)
        g.set(34 - step, y + 1)
        g.set(33 - step, y + 1)
        g.set(32 - step, y)
        g.set(31 - step, y)
    # Front paws: two small blocks under the head, on the base line.
    for x in (int(cx) - 7, int(cx) + 4):
        for dx in range(3):
            for dy in range(2):
                g.set(x + dx, 26 + dy)
    return g


def full() -> Grid:
    return cat(head_cy=14.0, head_r=10.5, eye_cy=12.0, eye_r=2.4)


def compact() -> Grid:
    """The same cat at 5 rows: fewer dots, same silhouette."""
    g = Grid(w=36, h=20)
    cx = 17.5
    head_cy, head_r = 10.0, 7.6
    crown = head_cy - head_r * 0.95
    g.ring(cx, head_cy, head_r, head_r * 0.95, thickness=1.3)
    g.triangle((11.5, 1.0), (9.0, crown), (15.0, crown), thickness=1)
    g.triangle((23.5, 1.0), (20.0, crown), (26.0, crown), thickness=1)
    eye_cy = 8.6
    g.disc(cx - 4.0, eye_cy, 1.8, 1.8)
    g.disc(cx + 4.0, eye_cy, 1.8, 1.8)
    nose_y = int(eye_cy + 3)
    g.stroke(cx - 1, nose_y, cx + 1, nose_y)
    for step, y in enumerate((nose_y - 2, nose_y + 1)):
        for dx in range(3):
            g.set(1 + step + dx, y)
            g.set(34 - step - dx, y)
    for x in (int(cx) - 6, int(cx) + 3):
        for dx in range(3):
            for dy in range(2):
                g.set(x + dx, 18 + dy)
    return g


def main():
    dots = "--dots" in sys.argv
    tiers = [("full", full())]
    if "--dots" not in sys.argv:
        tiers.append(("compact", compact()))
    for name, grid in tiers:
        print(f"--- {name} ---")
        print(grid.dots_view() if dots else grid.braille())
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
