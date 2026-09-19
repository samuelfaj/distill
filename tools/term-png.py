#!/usr/bin/env python3
"""Replay a captured escape stream into a PNG, so the TUI can be looked at.

`splash-capture.py` prints the *text* of the screen, which loses every colour and
turns the half-block art into a wall of identical glyphs. This replays the same
stream the way a terminal would — cursor motion, SGR colours, printable cells —
and rasterizes the grid.

Usage:
    SPLASH_RAW=/tmp/frame.bin python3 tools/splash-capture.py 42 132 20
    python3 tools/term-png.py /tmp/frame.bin /tmp/frame.png [cell_h_px] [cell_w_px]

Half-block cells (`▀`) paint two pixels from their own foreground/background,
which is what the mascot is made of; every other cell is painted as its
background with the glyph drawn in the foreground colour.
"""

import pathlib
import re
import struct
import sys
import zlib

# The 16 ANSI colours, for terminals that answer in indexed colour.
ANSI16 = [
    (0, 0, 0), (170, 0, 0), (0, 170, 0), (170, 85, 0),
    (0, 0, 170), (170, 0, 170), (0, 170, 170), (170, 170, 170),
    (85, 85, 85), (255, 85, 85), (85, 255, 85), (255, 255, 85),
    (85, 85, 255), (255, 85, 255), (85, 255, 255), (255, 255, 255),
]


def palette_256(index: int) -> tuple[int, int, int]:
    if index < 16:
        return ANSI16[index]
    if index < 232:
        index -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return (steps[index // 36], steps[(index // 6) % 6], steps[index % 6])
    level = 8 + (index - 232) * 10
    return (level, level, level)


class Screen:
    def __init__(self, rows: int, cols: int):
        self.rows, self.cols = rows, cols
        self.cells = [[(" ", (200, 200, 200), (0, 0, 0))] * cols for _ in range(rows)]
        self.row = self.col = 0
        self.fg = (200, 200, 200)
        self.bg = (0, 0, 0)

    def put(self, ch: str):
        if self.col >= self.cols:
            return
        self.cells[self.row][self.col] = (ch, self.fg, self.bg)
        self.col += 1


def apply_sgr(screen: Screen, params: list[int]):
    i = 0
    while i < len(params):
        p = params[i]
        if p == 0:
            screen.fg, screen.bg = (200, 200, 200), (0, 0, 0)
        elif p == 39:
            screen.fg = (200, 200, 200)
        elif p == 49:
            screen.bg = (0, 0, 0)
        elif p in (38, 48) and i + 1 < len(params):
            target = "fg" if p == 38 else "bg"
            if params[i + 1] == 2 and i + 4 < len(params):
                value = tuple(params[i + 2 : i + 5])
                i += 4
            elif params[i + 1] == 5 and i + 2 < len(params):
                value = palette_256(params[i + 2])
                i += 2
            else:
                value = None
            if value is not None:
                setattr(screen, target, value)
        i += 1


CSI = re.compile(rb"\x1b\[([0-9;?]*)([A-Za-z])")


def replay(raw: bytes, rows: int, cols: int) -> Screen:
    screen = Screen(rows, cols)
    i = 0
    text = raw
    while i < len(text):
        byte = text[i]
        if byte == 0x1B:
            match = CSI.match(text, i)
            if match:
                params = [int(p) for p in match.group(1).split(b";") if p.isdigit()]
                final = match.group(2)
                if final == b"H" or final == b"f":
                    row = (params[0] - 1) if params else 0
                    col = (params[1] - 1) if len(params) > 1 else 0
                    screen.row, screen.col = max(0, row), max(0, col)
                elif final == b"A":
                    screen.row = max(0, screen.row - (params[0] if params else 1))
                elif final == b"B":
                    screen.row = min(rows - 1, screen.row + (params[0] if params else 1))
                elif final == b"C":
                    screen.col = min(cols - 1, screen.col + (params[0] if params else 1))
                elif final == b"D":
                    screen.col = max(0, screen.col - (params[0] if params else 1))
                elif final == b"G":
                    screen.col = max(0, (params[0] - 1) if params else 0)
                elif final == b"J" and (not params or params[0] in (2, 3)):
                    screen.cells = [
                        [(" ", screen.fg, screen.bg)] * cols for _ in range(rows)
                    ]
                elif final == b"m":
                    apply_sgr(screen, params)
                i = match.end()
                continue
            # OSC (window title) or a two-byte escape: skip it.
            if text[i + 1 : i + 2] == b"]":
                end = text.find(b"\x07", i)
                i = (end + 1) if end != -1 else i + 2
            else:
                i += 2
            continue
        if byte == 0x0D:
            screen.col = 0
        elif byte == 0x0A:
            screen.row = min(rows - 1, screen.row + 1)
        elif byte == 0x08:
            screen.col = max(0, screen.col - 1)
        elif byte >= 0x20:
            try:
                ch = text[i:].decode("utf-8", errors="ignore")[0]
                length = len(ch.encode())
            except (IndexError, UnicodeDecodeError):
                i += 1
                continue
            screen.put(ch)
            i += length - 1
        i += 1
    return screen


def write_png(path: str, pixels, width: int, height: int):
    rows = []
    for y in range(height):
        rows.append(b"\x00" + b"".join(bytes(pixels[y][x]) for x in range(width)))
    raw = b"".join(rows)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(raw, 9))
    out += chunk(b"IEND", b"")
    pathlib.Path(path).write_bytes(out)


def rasterize(screen: Screen, cell_h: int, cell_w: int):
    width = screen.cols * cell_w
    height = screen.rows * cell_h
    pixels = [[(0, 0, 0)] * width for _ in range(height)]
    for y, row in enumerate(screen.cells):
        for x, (ch, fg, bg) in enumerate(row):
            top = fg if ch == "\u2580" else bg
            bottom = bg
            for dy in range(cell_h):
                for dx in range(cell_w):
                    # Half blocks split the cell; anything else draws a small
                    # blob of the foreground so text is visible as shape.
                    if ch == "\u2580":
                        colour = top if dy < cell_h / 2 else bottom
                    elif ch.strip():
                        inner = cell_h // 4
                        colour = fg if inner <= dy < cell_h - inner else bg
                    else:
                        colour = bg
                    pixels[y * cell_h + dy][x * cell_w + dx] = colour
    return pixels, width, height


def main() -> int:
    raw_path = sys.argv[1]
    out_path = sys.argv[2]
    cell_h = int(sys.argv[3]) if len(sys.argv) > 3 else 12
    cell_w = int(sys.argv[4]) if len(sys.argv) > 4 else 6
    rows = int(sys.argv[5]) if len(sys.argv) > 5 else 42
    cols = int(sys.argv[6]) if len(sys.argv) > 6 else 132

    screen = replay(pathlib.Path(raw_path).read_bytes(), rows, cols)
    pixels, width, height = rasterize(screen, cell_h, cell_w)
    write_png(out_path, pixels, width, height)
    print(f"{out_path}: {width}x{height} from {rows}x{cols} cells")
    return 0


if __name__ == "__main__":
    sys.exit(main())
