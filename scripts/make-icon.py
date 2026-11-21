#!/usr/bin/env python3
"""Renders termy's app icon (1024x1024 PNG) with no dependencies.

Design: a near-black rounded square (macOS icon grid: 824 px body) with a
light ">_" prompt and an accent cursor block, matching the Dark OLED theme.
Shapes are signed-distance fields, so edges are anti-aliased.
Usage: make-icon.py out.png
"""
import math, struct, sys, zlib

N = 1024
BODY, RADIUS = 824, 185          # Apple's icon template proportions
OFF = (N - BODY) / 2

def clamp(x, a=0.0, b=1.0):
    return a if x < a else b if x > b else x

def sd_round_rect(px, py, cx, cy, hw, hh, r):
    qx, qy = abs(px - cx) - hw + r, abs(py - cy) - hh + r
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r

def sd_segment(px, py, ax, ay, bx, by):
    pax, pay, bax, bay = px - ax, py - ay, bx - ax, by - ay
    h = clamp((pax * bax + pay * bay) / (bax * bax + bay * bay))
    return math.hypot(pax - bax * h, pay - bay * h)

def over(dst, src, a):
    return tuple(d * (1 - a) + s * a for d, s in zip(dst, src))

C = N / 2
chevron = [(372, 392), (492, 512), (372, 632)]
rows = []
for y in range(N):
    row = bytearray([0])
    for x in range(N):
        px, py = x + 0.5, y + 0.5
        d_body = sd_round_rect(px, py, C, C + 6, BODY / 2, BODY / 2, RADIUS)
        a_body = clamp(0.5 - d_body)
        if a_body <= 0:
            # Soft drop shadow below the body.
            d_sh = sd_round_rect(px, py, C, C + 22, BODY / 2, BODY / 2, RADIUS)
            a = clamp(0.28 * (1 - clamp(d_sh / 28)))
            row += bytes((0, 0, 0, int(a * 255)))
            continue
        t = (py - OFF) / BODY                       # vertical gradient
        col = (20 - 12 * t, 20 - 12 * t, 22 - 12 * t)
        col = over(col, (52, 52, 56), clamp(1.5 - abs(d_body + 2)) * 0.9)   # 1px rim
        stroke = 34
        d_chev = min(sd_segment(px, py, *chevron[0], *chevron[1]), sd_segment(px, py, *chevron[1], *chevron[2])) - stroke
        col = over(col, (222, 222, 222), clamp(0.5 - d_chev))
        d_under = sd_round_rect(px, py, 590, 636, 70, 30, 10)
        col = over(col, (222, 222, 222), clamp(0.5 - d_under))
        d_cursor = sd_round_rect(px, py, 712, 556, 30, 110, 8)
        col = over(col, (217, 119, 87), clamp(0.5 - d_cursor))
        row += bytes((int(col[0]), int(col[1]), int(col[2]), int(a_body * 255)))
    rows.append(bytes(row))

def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
