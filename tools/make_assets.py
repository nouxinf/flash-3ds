#!/usr/bin/env python3
"""Generates the icon, banner and banner sound for the 3DS packages.

Everything is drawn procedurally (no external image libraries) so the assets can
be rebuilt on any machine. Output: assets/icon.png, assets/banner.png,
assets/banner.wav.
"""
import math
import os
import struct
import zlib

W_ICON = 48
W_BANNER = (256, 128)


def png(path, width, height, pixels):
    raw = b"".join(b"\x00" + bytes(pixels[y]) for y in range(height))
    def chunk(kind, data):
        c = struct.pack(">I", len(data)) + kind + data
        return c + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(raw, 9))
    out += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(out)


def rounded(x, y, w, h, r):
    cx = min(max(x, r), w - 1 - r)
    cy = min(max(y, r), h - 1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def icon():
    w = h = W_ICON
    px = []
    for y in range(h):
        row = []
        for x in range(w):
            if not rounded(x, y, w, h, 8):
                row += [0x0E, 0x12, 0x17, 0]
                continue
            # Teal play triangle on a dark plate.
            tx = x - 16
            ty = y - 14
            inside = False
            if 10 <= tx <= 30 and 0 <= ty < 20:
                # Triangle: width shrinks with distance from the vertical centre.
                half = (20 - abs(ty - 10)) * 0.9
                inside = tx - 10 <= half * 1.4
            if inside:
                row += [0x4F, 0xD1, 0xC5, 255]
            elif 2 <= x < w - 2 and 2 <= y < h - 2:
                row += [0x16, 0x1C, 0x24, 255]
            else:
                row += [0x0E, 0x12, 0x17, 0]
        px.append(row)
    png("assets/icon.png", w, h, px)


def banner():
    w, h = W_BANNER
    px = []
    for y in range(h):
        row = []
        for x in range(w):
            t = x / (w - 1)
            r = int(0x0E + (0x10 - 0x0E) * t)
            g = int(0x12 + (0x2A - 0x12) * t)
            b = int(0x17 + (0x2E - 0x17) * t)
            # Accent bar and a simple film-strip pattern.
            if 18 <= x <= 22:
                r, g, b = 0x4F, 0xD1, 0xC5
            if y in (14, h - 15) and x > 34:
                r, g, b = 0x1F, 0x6F, 0x78
            for i in range(3):
                cx, cy = 40 + i * 26, h // 2
                if abs(x - cx) < 3 and abs(y - cy) < 9:
                    r, g, b = 0x4F, 0xD1, 0xC5
            row += [r, g, b, 255]
        px.append(row)
    png("assets/banner.png", w, h, px)


def wav(path):
    rate = 32000
    n = rate // 2
    with open(path, "wb") as f:
        f.write(b"RIFF")
        f.write(struct.pack("<I", 36 + n * 2))
        f.write(b"WAVEfmt ")
        f.write(struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16))
        f.write(b"data")
        f.write(struct.pack("<I", n * 2))
        f.write(b"\x00\x00" * n)
    _ = math


if __name__ == "__main__":
    os.makedirs("assets", exist_ok=True)
    icon()
    banner()
    wav("assets/banner.wav")
    print("wrote assets/icon.png, assets/banner.png, assets/banner.wav")
