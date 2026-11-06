#!/usr/bin/env python3
"""Write icon.png (256x256) and icon.ico (the same image, PNG-compressed inside the ICO) next to
this script: a slate rounded square with an orange crosshair. Pure stdlib, so the committed icons
can be regenerated anywhere: python3 -I make-icons.py
"""
import math
import pathlib
import struct
import zlib

N = 256
BG = (38, 44, 56)
FG = (240, 140, 40)


def coverage(x, y):
    """(background alpha, foreground alpha) at pixel centre x, y, 4x4 supersampled."""
    bg = fg = 0
    for sy in range(4):
        for sx in range(4):
            px, py = x + (sx + 0.5) / 4, y + (sy + 0.5) / 4
            # rounded square, radius 40
            r, m = 40, 8
            qx = max(m + r - px, 0, px - (N - m - r))
            qy = max(m + r - py, 0, py - (N - m - r))
            inside = m <= px <= N - m and m <= py <= N - m and math.hypot(qx, qy) <= r
            if not inside:
                continue
            bg += 1
            cx, cy = px - N / 2, py - N / 2
            d = math.hypot(cx, cy)
            ring = 62 <= d <= 76
            dot = d <= 10
            arm = (abs(cx) <= 7 and 30 <= abs(cy) <= 104) or (abs(cy) <= 7 and 30 <= abs(cx) <= 104)
            if ring or dot or arm:
                fg += 1
    return bg / 16, fg / 16


def png(width, height, rows):
    raw = b"".join(b"\x00" + bytes(r) for r in rows)

    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    rows = []
    for y in range(N):
        row = []
        for x in range(N):
            a_bg, a_fg = coverage(x, y)
            rgb = [round(BG[i] * (1 - a_fg) + FG[i] * a_fg) for i in range(3)]
            row += rgb + [round(255 * a_bg)]
        rows.append(row)
    data = png(N, N, rows)
    here = pathlib.Path(__file__).resolve().parent
    (here / "icon.png").write_bytes(data)
    # ICONDIR + one ICONDIRENTRY (0 = 256 px) + the PNG (Vista+ reads PNG-compressed entries)
    ico = struct.pack("<HHH", 0, 1, 1) + struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(data), 6 + 16) + data
    (here / "icon.ico").write_bytes(ico)


main()
