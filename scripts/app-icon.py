#!/usr/bin/env python3
"""Regenerate the app icon sources in assets/logo from the year-rings mark.

Writes two 1024 px SVGs:
  varve-app-icon-macos.svg  the tile on Apple's icon grid (824 px with a soft shadow), for icon.icns
  varve-app-icon.svg        the tile filling the canvas, for Windows, Linux and the other PNG sizes

and the Icon Composer document Varve.icon (icon.json and its layer SVGs in Assets/), the Liquid Glass
icon for macOS 26 and later. scripts/liquid-glass-icon.sh compiles it into Assets.car.

Then render them and hand them to the Tauri CLI, and rerun scripts/readme-images.py (the README
banner draws the same tile). The README, under "Releases", lists the exact commands.
"""

import json
import math
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


# Liquid Glass (macOS 26 and later): an Icon Composer document, Varve.icon. The system draws the
# tile shape, the glass, the specular light and the shadows, and makes the dark, clear and tinted
# appearances, so the layers carry only flat art: the tile gradient becomes the document fill, and
# each ring and the dot become a layer of their own. Icon Composer's canvas is 1024 points with the
# tile filling it, so the mark is the 512 box above at twice the size, the same as full_bleed().
# scripts/liquid-glass-icon.sh compiles the document to app/src-tauri/macos/Assets.car.

GLASS_SCALE = 2
GLASS_CANVAS = 512 * GLASS_SCALE
RING_ROTATION = -62  # degrees, as in tile()
DOT_RADIUS = 28


def color(hex_rgb: str, alpha: float = 1.0) -> str:
    # Icon Composer writes colors as "<color space>:r,g,b,a" with five decimals.
    r, g, b = (int(hex_rgb[i:i + 2], 16) / 255 for i in (1, 3, 5))
    return f'extended-srgb:{r:.5f},{g:.5f},{b:.5f},{alpha:.5f}'


def ring_path(radius: float, width: float, dash: str) -> str:
    """The visible dash of one ring as a filled outline (round caps included), in canvas units.

    SVG draws a dashed circle from 3 o'clock, clockwise on screen, for the first dash length; tile()
    then rotates it. A filled path keeps the layer free of stroke dashing, which Icon Composer's
    renderer then has no say in.
    """
    s = GLASS_SCALE
    c = GLASS_CANVAS / 2
    r, half = radius * s, width * s / 2
    dash_len = float(dash.split()[0]) * s
    sweep = math.degrees(dash_len / r)  # arc length to degrees on the ring's center line
    a0 = math.radians(RING_ROTATION)
    a1 = math.radians(RING_ROTATION + sweep)
    large = 1 if sweep > 180 else 0

    def pt(rr: float, a: float) -> str:
        return f'{c + rr * math.cos(a):.3f} {c + rr * math.sin(a):.3f}'

    ro, ri = r + half, r - half
    return (f'M {pt(ro, a0)} '
            f'A {ro:.3f} {ro:.3f} 0 {large} 1 {pt(ro, a1)} '
            f'A {half:.3f} {half:.3f} 0 0 1 {pt(ri, a1)} '
            f'A {ri:.3f} {ri:.3f} 0 {large} 0 {pt(ri, a0)} '
            f'A {half:.3f} {half:.3f} 0 0 1 {pt(ro, a0)} Z')


# Per ring: the gradient stops of tile() and its line, both in the 512 box.
GLASS_RING_GRADIENTS = {
    'ring-outer': ((96, 416), (('0', '#C9C1FF'), ('0.45', '#8B7CFF'), ('1', '#5B4BE0'))),
    'ring-middle': ((138, 374), (('0', '#FFFFFF'), ('1', '#C3C8D1'))),
    'ring-inner': ((180, 332), (('0', '#C2C7D0'), ('1', '#7A8190'))),
}


def glass_svg(body: str) -> str:
    n = GLASS_CANVAS
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{n}" height="{n}" viewBox="0 0 {n} {n}">\n'
            f'  {body}\n</svg>\n')


def glass_ring(radius: float, width: float, dash: str, gid: str) -> str:
    (start, end), stops = GLASS_RING_GRADIENTS[gid]
    s = GLASS_SCALE
    c = GLASS_CANVAS / 2
    # tile() draws the gradient inside the rotated group, so it turns with the ring.
    stop_tags = ''.join(f'<stop offset="{o}" stop-color="{col}"/>' for o, col in stops)
    return glass_svg(
        f'<defs><linearGradient id="g" gradientUnits="userSpaceOnUse" x1="{start * s}" y1="{start * s}" '
        f'x2="{end * s}" y2="{end * s}" gradientTransform="rotate({RING_ROTATION} {c:g} {c:g})">'
        f'{stop_tags}</linearGradient></defs>\n'
        f'  <path fill="url(#g)" d="{ring_path(radius, width, dash)}"/>')


def glass_dot() -> str:
    c = GLASS_CANVAS / 2
    return glass_svg(
        '<defs><radialGradient id="g" cx="0.38" cy="0.32" r="0.75">'
        '<stop offset="0" stop-color="#A9AFBA"/><stop offset="1" stop-color="#4A5160"/></radialGradient></defs>\n'
        f'  <circle cx="{c:g}" cy="{c:g}" r="{DOT_RADIUS * GLASS_SCALE}" fill="url(#g)"/>')


def glass_glow() -> str:
    # The violet glow of tile() behind the rings, kept off the glass so it reads as light in the tile.
    c = GLASS_CANVAS / 2
    return glass_svg(
        '<defs><radialGradient id="g" cx="0.5" cy="0.5" r="0.5">'
        '<stop offset="0" stop-color="#8B7CFF" stop-opacity="0.34"/>'
        '<stop offset="0.6" stop-color="#5B4BE0" stop-opacity="0.10"/>'
        '<stop offset="1" stop-color="#5B4BE0" stop-opacity="0"/></radialGradient></defs>\n'
        f'  <circle cx="{c:g}" cy="{c:g}" r="{250 * GLASS_SCALE}" fill="url(#g)"/>')


def glass_layer(name: str, image: str) -> dict:
    return {'image-name': image, 'name': name, 'glass': True}


def glass_document() -> tuple[dict, dict[str, str]]:
    """The icon.json of Varve.icon and its Assets/ files, keyed by file name.

    Schema as Icon Composer writes it (see the Xcode template
    Templates/File Templates/macOS/Resource/Icon Composer Icon.xctemplate): groups and layers list
    the front-most first.
    """
    assets = {f'{gid}.svg': glass_ring(r, w, dash, gid) for r, w, dash, gid in RINGS}
    assets['dot.svg'] = glass_dot()
    assets['glow.svg'] = glass_glow()
    document = {
        'fill': {
            # tile(): #262A33 at the top to #0C0E12 at the bottom.
            'linear-gradient': [color('#262A33'), color('#0C0E12')],
            'orientation': {'start': {'x': 0.5, 'y': 0}, 'stop': {'x': 0.5, 'y': 1}},
        },
        'groups': [
            {
                'name': 'Mark',
                'layers': [
                    glass_layer('Dot', 'dot.svg'),
                    glass_layer('Inner ring', 'ring-inner.svg'),
                    glass_layer('Middle ring', 'ring-middle.svg'),
                    glass_layer('Outer ring', 'ring-outer.svg'),
                ],
                'lighting': 'individual',
                'specular': True,
                'shadow': {'kind': 'layer-color', 'opacity': 0.5},
                'translucency': {'enabled': True, 'value': 0.3},
            },
            {
                'name': 'Glow',
                'layers': [{'image-name': 'glow.svg', 'name': 'Glow', 'glass': False}],
                'shadow': {'kind': 'none', 'opacity': 0.5},
                'translucency': {'enabled': False, 'value': 0.5},
                'specular': False,
            },
        ],
        'supported-platforms': {'squares': ['macOS']},
    }
    return document, assets


def write_glass_document(target: Path) -> None:
    document, assets = glass_document()
    (target / 'Assets').mkdir(parents=True, exist_ok=True)
    # Icon Composer's own formatting: two-space indent and " : " between key and value.
    text = json.dumps(document, indent=2).replace('": ', '" : ')
    (target / 'icon.json').write_text(text + '\n')
    for name, body in assets.items():
        (target / 'Assets' / name).write_text(body)


def main() -> None:
    (OUT / 'varve-app-icon-macos.svg').write_text(macos())
    (OUT / 'varve-app-icon.svg').write_text(full_bleed())
    write_glass_document(OUT / 'Varve.icon')
    print(f'wrote {OUT}')


if __name__ == '__main__':
    main()
