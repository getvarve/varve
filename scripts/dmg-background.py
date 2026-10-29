#!/usr/bin/env python3
"""Regenerate the macOS installer window background: app/src-tauri/dmg/background.png.

The window is 660 x 400 points (tauri.conf.json, bundle.macOS.dmg). The PNG is rendered at twice
that size and marked 144 DPI, so Finder shows it at 660 x 400 points and sharp on Retina screens.
The footer stays well above the bottom edge: Finder's window bounds may include the title bar, so
the lowest ~28 points of the image may sit outside the visible area.
Requires rsvg-convert (librsvg); fonts fall back to what it finds, so another machine may produce a
slightly different PNG. Run: python3 scripts/dmg-background.py
"""

import json
import struct
import subprocess
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / 'app' / 'src-tauri' / 'dmg' / 'background.png'

# The window size and icon centers (in points) come from tauri.conf.json, so they cannot drift.
_DMG = json.loads((ROOT / 'app' / 'src-tauri' / 'tauri.conf.json').read_text())['bundle']['macOS']['dmg']
WIDTH, HEIGHT = _DMG['windowSize']['width'], _DMG['windowSize']['height']
APP = (_DMG['appPosition']['x'], _DMG['appPosition']['y'])
APPLICATIONS = (_DMG['applicationFolderPosition']['x'], _DMG['applicationFolderPosition']['y'])

# Violet slate with relative luminance 0.178: 4.57:1 against black text and 4.60:1 against white.
LABEL_GROUND = '#74738B'

SANS = "'IBM Plex Sans', -apple-system, BlinkMacSystemFont, 'Helvetica Neue', Helvetica, Arial, sans-serif"


def svg() -> str:
    ax, ay = APP
    bx, by = APPLICATIONS
    # The arrow runs between the two 128 pt icons with room to spare, arching a little upward.
    x1, x2, y = ax + 92, bx - 98, ay - 6
    mid = (x1 + x2) / 2
    # Finder draws the icon labels black in light mode and white in dark mode, over this image.
    # LABEL_GROUND has a relative luminance of 0.178, which gives both 4.6:1 contrast (WCAG AA), so
    # the labels read in either mode. The band behind the labels stays that exact flat color; the
    # glow sits above the icons, never behind a label.
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <defs>
    <radialGradient id="glow" cx="0.5" cy="0.5" r="0.5"><stop offset="0" stop-color="#8B7CFF" stop-opacity="0.38"/><stop offset="1" stop-color="#8B7CFF" stop-opacity="0"/></radialGradient>
    <pattern id="dots" width="18" height="18" patternUnits="userSpaceOnUse"><circle cx="2" cy="2" r="0.9" fill="#FFFFFF" fill-opacity="0.06"/></pattern>
    <marker id="head" viewBox="0 0 20 20" refX="12" refY="10" markerWidth="20" markerHeight="20" markerUnits="userSpaceOnUse" orient="auto">
      <path d="M2 2.5 L13 10 L2 17.5" fill="none" stroke="#FFFFFF" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"/>
    </marker>
    <linearGradient id="arrow" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.35"/><stop offset="1" stop-color="#FFFFFF"/>
    </linearGradient>
    <clipPath id="above-labels"><rect width="{WIDTH}" height="{ay + 50}"/></clipPath>
  </defs>
  <rect width="{WIDTH}" height="{HEIGHT}" fill="{LABEL_GROUND}"/>
  <g clip-path="url(#above-labels)">
    <ellipse cx="{ax}" cy="{ay - 40}" rx="140" ry="90" fill="url(#glow)"/>
    <ellipse cx="{bx}" cy="{by - 40}" rx="140" ry="90" fill="url(#glow)"/>
    <rect width="{WIDTH}" height="{ay + 50}" fill="url(#dots)"/>
  </g>
  <text x="{WIDTH / 2}" y="58" text-anchor="middle" font-family="{SANS}" font-size="19" font-weight="600" fill="#FFFFFF">Drag Varve to Applications</text>
  <text x="{WIDTH / 2}" y="82" text-anchor="middle" font-family="{SANS}" font-size="12.5" font-weight="500" fill="#FFFFFF">Then open it from your Applications folder or Spotlight.</text>
  <path d="M{x1} {y} Q {mid} {y - 34} {x2} {y}" fill="none" stroke="url(#arrow)" stroke-width="3" stroke-linecap="round" stroke-dasharray="0.1 9" marker-end="url(#head)"/>
  <text x="{WIDTH / 2}" y="{HEIGHT - 52}" text-anchor="middle" font-family="{SANS}" font-size="11" font-weight="500" fill="#FFFFFF">Varve  ·  github.com/getvarve/varve</text>
</svg>
'''


def with_dpi(png: bytes, dpi: int) -> bytes:
    """Returns the PNG with a pHYs chunk for the given DPI, right after IHDR."""
    assert png[:8] == b'\x89PNG\r\n\x1a\n'
    ihdr_end = 8 + 8 + struct.unpack('>I', png[8:12])[0] + 4
    chunks, pos = [], ihdr_end
    while pos < len(png):
        length = struct.unpack('>I', png[pos:pos + 4])[0]
        kind = png[pos + 4:pos + 8]
        end = pos + 12 + length
        if kind != b'pHYs':
            chunks.append(png[pos:end])
        pos = end
    per_metre = round(dpi / 0.0254)
    body = b'pHYs' + struct.pack('>IIB', per_metre, per_metre, 1)
    phys = struct.pack('>I', 9) + body + struct.pack('>I', zlib.crc32(body) & 0xFFFFFFFF)
    return png[:ihdr_end] + phys + b''.join(chunks)


def main() -> None:
    OUT.parent.mkdir(parents=True, exist_ok=True)
    png = subprocess.run(
        ['rsvg-convert', '--width', str(WIDTH * 2), '--height', str(HEIGHT * 2), '--format', 'png'],
        input=svg().encode(), capture_output=True, check=True,
    ).stdout
    OUT.write_bytes(with_dpi(png, 144))
    print(f'wrote {OUT}')


if __name__ == '__main__':
    main()
