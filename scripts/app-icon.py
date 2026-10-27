#!/usr/bin/env python3
"""Regenerate the app icon sources in assets/logo from the year-rings mark.

Writes two 1024 px SVGs:
  varve-app-icon-macos.svg  the tile on Apple's icon grid (824 px with a soft shadow), for icon.icns
  varve-app-icon.svg        the tile filling the canvas, for Windows, Linux and the other PNG sizes

Then render them and hand them to the Tauri CLI, and rerun scripts/readme-images.py (the README
banner draws the same tile). The README, under "Releases", lists the exact commands.
"""

from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / 'assets' / 'logo'

# The mark's geometry, unchanged from varve-icon.svg (a 512 box): three open rings rotated -62deg
# and a center dot. Only the finish changes: depth, light and a glow on the violet ring.
RINGS = [
    # radius, stroke width, dasharray, gradient id
    (172, 44, '900 1081', 'ring-outer'),
    (118, 30, '618 741', 'ring-middle'),
    (76, 20, '398 478', 'ring-inner'),
]


def tile(defs_prefix: str) -> str:
    p = defs_prefix
    rings = ''.join(
        f'<circle cx="256" cy="256" r="{r}" stroke="url(#{p}{gid})" stroke-width="{w}" stroke-dasharray="{dash}"/>'
        for r, w, dash, gid in RINGS
    )
    outer = RINGS[0]
    return f'''<defs>
    <linearGradient id="{p}tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#262A33"/><stop offset="0.55" stop-color="#16191F"/><stop offset="1" stop-color="#0C0E12"/>
    </linearGradient>
    <radialGradient id="{p}glow" cx="0.5" cy="0.5" r="0.5">
      <stop offset="0" stop-color="#8B7CFF" stop-opacity="0.34"/><stop offset="0.6" stop-color="#5B4BE0" stop-opacity="0.10"/><stop offset="1" stop-color="#5B4BE0" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="{p}sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.10"/><stop offset="0.45" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="{p}edge" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.22"/><stop offset="0.5" stop-color="#FFFFFF" stop-opacity="0.04"/><stop offset="1" stop-color="#FFFFFF" stop-opacity="0.10"/>
    </linearGradient>
    <linearGradient id="{p}ring-outer" gradientUnits="userSpaceOnUse" x1="96" y1="96" x2="416" y2="416">
      <stop offset="0" stop-color="#C9C1FF"/><stop offset="0.45" stop-color="#8B7CFF"/><stop offset="1" stop-color="#5B4BE0"/>
    </linearGradient>
    <linearGradient id="{p}ring-middle" gradientUnits="userSpaceOnUse" x1="138" y1="138" x2="374" y2="374">
      <stop offset="0" stop-color="#FFFFFF"/><stop offset="1" stop-color="#C3C8D1"/>
    </linearGradient>
    <linearGradient id="{p}ring-inner" gradientUnits="userSpaceOnUse" x1="180" y1="180" x2="332" y2="332">
      <stop offset="0" stop-color="#C2C7D0"/><stop offset="1" stop-color="#7A8190"/>
    </linearGradient>
    <radialGradient id="{p}dot" cx="0.38" cy="0.32" r="0.75">
      <stop offset="0" stop-color="#A9AFBA"/><stop offset="1" stop-color="#4A5160"/>
    </radialGradient>
    <filter id="{p}bloom" x="-30%" y="-30%" width="160%" height="160%">
      <feGaussianBlur stdDeviation="14"/>
    </filter>
    <filter id="{p}lift" x="-20%" y="-20%" width="140%" height="140%">
      <feGaussianBlur in="SourceAlpha" stdDeviation="7"/><feOffset dy="8" result="blur"/>
      <feFlood flood-color="#000000" flood-opacity="0.55"/><feComposite in2="blur" operator="in"/>
      <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <clipPath id="{p}clip"><rect width="512" height="512" rx="114"/></clipPath>
  </defs>
  <g clip-path="url(#{p}clip)">
    <rect width="512" height="512" fill="url(#{p}tile)"/>
    <circle cx="256" cy="256" r="250" fill="url(#{p}glow)"/>
    <g transform="rotate(-62 256 256)" fill="none" stroke-linecap="round">
      <circle cx="256" cy="256" r="{outer[0]}" stroke="#8B7CFF" stroke-opacity="0.55" stroke-width="{outer[1]}" stroke-dasharray="{outer[2]}" filter="url(#{p}bloom)"/>
      <g filter="url(#{p}lift)">{rings}</g>
    </g>
    <circle cx="256" cy="256" r="28" fill="url(#{p}dot)" filter="url(#{p}lift)"/>
    <rect width="512" height="512" fill="url(#{p}sheen)"/>
  </g>
  <rect x="1" y="1" width="510" height="510" rx="113" fill="none" stroke="url(#{p}edge)" stroke-width="2"/>'''


def svg(body: str) -> str:
    return ('<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024" '
            'role="img" aria-label="Varve">\n  <title>Varve</title>\n  ' + body + '\n</svg>\n')


def macos() -> str:
    # Apple's grid: an 824 px tile centered on the 1024 canvas, 100 px margin, shadow below it.
    scale = 824 / 512
    return svg(f'''<defs><filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
    <feGaussianBlur in="SourceAlpha" stdDeviation="14"/><feOffset dy="12" result="blur"/>
    <feFlood flood-color="#000000" flood-opacity="0.32"/><feComposite in2="blur" operator="in"/>
  </filter></defs>
  <rect x="100" y="100" width="824" height="824" rx="{114 * scale:.1f}" filter="url(#shadow)"/>
  <g transform="translate(100 100) scale({scale:.6f})">
  {tile('m-')}
  </g>''')


def full_bleed() -> str:
    return svg(f'''<g transform="scale(2)">
  {tile('f-')}
  </g>''')


def main() -> None:
    (OUT / 'varve-app-icon-macos.svg').write_text(macos())
    (OUT / 'varve-app-icon.svg').write_text(full_bleed())
    print(f'wrote {OUT}')


if __name__ == '__main__':
    main()
