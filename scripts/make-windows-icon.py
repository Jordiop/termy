#!/usr/bin/env python3
"""Derives the Windows / Linux icons from the macOS one (needs Pillow).

The macOS icon follows Apple's grid: an 824 px body centred in 1024 px with
a drop shadow around it. Windows icons are expected to fill their square, so
the body is cropped out (no margin, no shadow) before scaling down.

Writes:
  assets/windows/termy.ico   all the sizes Windows asks for (embedded in the .exe)
  assets/windows/icon-256.png the window / taskbar icon set at runtime

Usage: scripts/make-windows-icon.py   (run from the repo root)
"""
from pathlib import Path

from PIL import Image

SRC = Path("assets/macos/icon-1024.png")
OUT = Path("assets/windows")
# Body square in make-icon.py: 824 px, centred at (512, 518).
BODY = (100, 106, 924, 930)
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]

def main() -> None:
    body = Image.open(SRC).convert("RGBA").crop(BODY)
    OUT.mkdir(parents=True, exist_ok=True)
    big = body.resize((256, 256), Image.LANCZOS)
    big.save(OUT / "icon-256.png", optimize=True)
    big.save(OUT / "termy.ico", sizes=[(s, s) for s in ICO_SIZES])
    print(f"wrote {OUT / 'termy.ico'} and {OUT / 'icon-256.png'}")

if __name__ == "__main__":
    main()
