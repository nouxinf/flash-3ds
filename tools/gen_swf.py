#!/usr/bin/env python3
"""Generates a SWF test movie for the flash3ds renderer.

Written from the SWF file-format specification, independently of the Rust
decoder, so a disagreement between the two shows up as a wrong image. Output is
a plain uncompressed FWS file with two shapes, a stroke and a three-frame
timeline (frame 2 moves one shape).

Usage: python tools/gen_swf.py out.swf
"""
import struct
import sys


class BitWriter:
    def __init__(self):
        self.bits = []

    def bit(self, v):
        self.bits.append(v & 1)

    def ub(self, v, n):
        for i in range(n - 1, -1, -1):
            self.bit((v >> i) & 1)

    def sb(self, v, n):
        if v < 0:
            v += 1 << n
        self.ub(v, n)

    def le16(self, v):
        # Byte-aligned integers in SWF are little-endian; only bit fields are
        # MSB-first.
        self.ub(v & 0xFF, 8)
        self.ub((v >> 8) & 0xFF, 8)

    def align(self):
        while len(self.bits) % 8:
            self.bits.append(0)

    def take(self):
        self.align()
        out = bytearray()
        for i in range(0, len(self.bits), 8):
            b = 0
            for j in range(8):
                b = (b << 1) | self.bits[i + j]
            out.append(b)
        self.bits = []
        return bytes(out)


def nbits_for(values):
    n = 1
    for v in values:
        while not (-(1 << (n - 1)) <= v <= (1 << (n - 1)) - 1):
            n += 1
    return n


def rect(w, h, bw):
    bw.ub(15, 5)
    for v in (0, w, 0, h):
        bw.sb(v, 15)


def tag(code, payload):
    if len(payload) >= 0x3F:
        return struct.pack("<HI", (code << 6) | 0x3F, len(payload)) + payload
    return struct.pack("<H", (code << 6) | len(payload)) + payload


def shape_tag(code, shape_id, w, h, color, outline, poly):
    bw = BitWriter()
    bw.le16(shape_id)
    rect(w, h, bw)
    bw.align()  # RECT is followed by byte-aligned data
    bw.ub(1, 8)  # one fill style
    bw.ub(0x00, 8)
    bw.ub(color[0], 8)
    bw.ub(color[1], 8)
    bw.ub(color[2], 8)
    bw.ub(0xFF, 8)
    bw.ub(1, 8)  # one line style
    bw.le16(40)  # 2px
    bw.ub(outline[0], 8)
    bw.ub(outline[1], 8)
    bw.ub(outline[2], 8)
    bw.ub(0xFF, 8)
    bw.ub(1, 4)  # fill bits
    bw.ub(1, 4)  # line bits

    # Move to the first point, then select fill 1 + line 1.
    n = nbits_for([p[0] for p in poly] + [p[1] for p in poly])
    bw.bit(0)
    bw.ub(0x01, 5)
    bw.ub(n, 5)  # MoveTo NumBits has no +2 bias; edge records do
    bw.sb(poly[0][0], n)
    bw.sb(poly[0][1], n)

    bw.bit(0)
    bw.ub(0x0C, 5)  # fill1 + line
    bw.ub(1, 1)
    bw.ub(1, 1)

    # Straight edges around the polygon.
    for i in range(1, len(poly) + 1):
        x0, y0 = poly[i - 1]
        x1, y1 = poly[i % len(poly)]
        dx, dy = x1 - x0, y1 - y0
        en = nbits_for([dx, dy])
        bw.bit(1)
        bw.bit(1)
        bw.ub(en - 2, 4)
        bw.bit(1)
        bw.sb(dx, en)
        bw.sb(dy, en)

    # End of shape records.
    bw.bit(0)
    bw.ub(0, 5)
    return tag(code, bw.take())


def place(depth, character, tx, ty, move=False):
    bw = BitWriter()
    flags = 0x04 | (0x01 if move else 0x02)
    bw.ub(flags, 8)
    bw.le16(depth)
    if not move:
        bw.le16(character)
    bw.bit(0)  # no scale
    bw.bit(0)  # no rotate
    tn = nbits_for([tx, ty])
    bw.ub(tn, 5)
    bw.sb(tx, tn)
    bw.sb(ty, tn)
    return tag(26, bw.take())


def main(path):
    W, H = 320, 240
    body = bytearray()
    body += tag(9, bytes([0x14, 0x1A, 0x22]))  # background

    # A rectangle, in twips, drawn as a closed polygon with an outline.
    rect_poly = [(0, 0), (2400, 0), (2400, 1800), (0, 1800)]
    body += shape_tag(32, 1, 2400, 1800, (0xE5, 0x50, 0x4A), (0x20, 0x10, 0x10), rect_poly)

    # A five-point star, to exercise non-convex even-odd filling.
    import math
    star = []
    for i in range(10):
        ang = -math.pi / 2 + i * math.pi / 5
        r = 900 if i % 2 == 0 else 380
        star.append((int(900 + r * math.cos(ang)), int(900 + r * math.sin(ang))))
    body += shape_tag(32, 2, 1800, 1800, (0x4F, 0xD1, 0xC5), (0x0A, 0x28, 0x28), star)

    body += place(1, 1, 1600, 1400)
    body += place(2, 2, 4800, 1200)
    body += tag(1, b"")  # show frame
    body += place(1, 0, 3200, 2800, move=True) + tag(1, b"")
    body += tag(1, b"")  # third frame, unchanged
    body += tag(0, b"")

    bw = BitWriter()
    rect(W * 20, H * 20, bw)
    header = bytearray(b"FWS")
    header.append(6)
    header += b"\x00\x00\x00\x00"  # length patched below
    header += bw.take()
    header += struct.pack("<H", 12 << 8)  # 12 fps
    header += struct.pack("<H", 3)  # frames

    out = bytes(header) + bytes(body)
    out = out[:4] + struct.pack("<I", len(out)) + out[8:]
    with open(path, "wb") as f:
        f.write(out)
    print(f"wrote {path} ({len(out)} bytes)")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "test.swf")
