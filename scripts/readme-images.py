#!/usr/bin/env python3
"""Regenerate the README banner and how-it-works diagram in assets/readme (light and dark).

Edit the content in banner() / how_it_works() or the THEMES palette, then run:
    python3 scripts/readme-images.py
"""

from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / 'assets' / 'readme'

SANS = "'IBM Plex Sans', -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "'IBM Plex Mono', ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace"

# The app's own tokens (packages/ui/src/styles/tokens.css): neutral grays, one violet accent,
# and red only where something is dangerous. The images follow the same rule.
THEMES = {
    'light': dict(
        ground='#F6F7F9', surface='#FFFFFF', raised='#EEF0F3', border='#D9DDE3', inner='#E6E9ED',
        text='#14171C', muted='#4A5160', subtle='#6A7180', faint='#9AA0AB',
        accent='#5B4BE0', accent_tint='#EAE7FC', accent_text='#4637C4',
        danger='#D93A37', danger_tint='#FDE4E3', danger_text='#B42724',
        shadow='#14171C', shadow_op='0.07', glow_op='0.14', dot_op='0.07',
    ),
    'dark': dict(
        ground='#0F1115', surface='#161920', raised='#1D212A', border='#2A2F3A', inner='#1D212A',
        text='#E6E8EC', muted='#A3A9B5', subtle='#7A8190', faint='#5C6270',
        accent='#8B7CFF', accent_tint='#262243', accent_text='#B3A9FF',
        danger='#F0605D', danger_tint='#3A1E21', danger_text='#F48A87',
        shadow='#000000', shadow_op='0.45', glow_op='0.20', dot_op='0.06',
    ),
}


def mark(x: float, y: float, size: float) -> str:
    """The app icon (assets/logo/varve-icon.svg): year rings on a dark tile, the same in both themes."""
    scale = size / 512
    return (
        f'<g transform="translate({x} {y}) scale({scale:.5f})">'
        '<rect width="512" height="512" rx="114" fill="#14171C"/>'
        '<g fill="none" stroke-linecap="round" transform="rotate(-62 256 256)">'
        '<circle cx="256" cy="256" r="172" stroke="#8B7CFF" stroke-width="44" stroke-dasharray="900 1081"/>'
        '<circle cx="256" cy="256" r="118" stroke="#E6E8EC" stroke-width="30" stroke-dasharray="618 741"/>'
        '<circle cx="256" cy="256" r="76" stroke="#A3A9B5" stroke-width="20" stroke-dasharray="398 478"/>'
        '</g><circle cx="256" cy="256" r="28" fill="#6A7180"/></g>'
    )


def text(x, y, content, t, size=14, weight=400, fill='text', family=SANS, anchor='start', extra=''):
    color = t.get(fill, fill)
    return (f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" '
            f'fill="{color}" text-anchor="{anchor}"{extra}>{content}</text>')


def frame(width: int, height: int, t: dict, label: str, body: str) -> str:
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="{label}">
  <title>{label}</title>
  <defs>
    <radialGradient id="glow"><stop offset="0" stop-color="{t['accent']}" stop-opacity="{t['glow_op']}"/><stop offset="1" stop-color="{t['accent']}" stop-opacity="0"/></radialGradient>
    <pattern id="dots" width="22" height="22" patternUnits="userSpaceOnUse"><circle cx="2" cy="2" r="1.2" fill="{t['accent']}" fill-opacity="{t['dot_op']}"/></pattern>
    <clipPath id="clip"><rect width="{width}" height="{height}" rx="24"/></clipPath>
  </defs>
  <g clip-path="url(#clip)">
    <rect width="{width}" height="{height}" fill="{t['ground']}"/>
    <rect width="{width}" height="{height}" fill="url(#dots)"/>
    <circle cx="{width - 280}" cy="{height // 2}" r="340" fill="url(#glow)"/>
    <circle cx="90" cy="0" r="220" fill="url(#glow)"/>
  </g>
  <rect x="0.75" y="0.75" width="{width - 1.5}" height="{height - 1.5}" rx="23.25" fill="none" stroke="{t['border']}" stroke-width="1.5"/>
{body}
</svg>
'''


def card(x, y, w, h, t, radius=12, fill='surface', stroke='border', dash=''):
    dash_attr = f' stroke-dasharray="{dash}"' if dash else ''
    return (f'<rect x="{x}" y="{y + 5}" width="{w}" height="{h}" rx="{radius}" fill="{t["shadow"]}" fill-opacity="{t["shadow_op"]}"/>'
            f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{radius}" fill="{t[fill]}" stroke="{t[stroke]}" stroke-width="1.25"{dash_attr}/>')


def banner(t: dict) -> str:
    parts = []
    parts.append(text(72, 92, 'POSTGRESQL  ·  MYSQL  ·  MARIADB  ·  REDIS', t, 13, 600, 'accent', extra=' letter-spacing="3"'))
    parts.append(mark(72, 116, 76))
    parts.append(text(166, 180, 'Varve', t, 80, 700, extra=' letter-spacing="-2"'))
    parts.append(text(72, 232, 'A calm desktop manager for your databases.', t, 23, 500, 'muted'))

    command = '$ curl -fsSL https://sherin.dev/varve/install.sh | bash'
    # 36 px left of the text, 24 px right of it: the rounded end needs the extra room.
    pill_w = 36 + len(command) * 8.7 + 24
    parts.append(f'<rect x="72" y="260" width="{pill_w:.1f}" height="40" rx="20" fill="{t["surface"]}" stroke="{t["border"]}"/>')
    parts.append(f'<circle cx="93" cy="280" r="4.5" fill="{t["accent"]}"/>')
    parts.append(text(108, 285, command, t, 14.5, 500, 'muted', MONO))

    # The app window: a production connection gets the red bar, nothing else is colored.
    wx, wy, ww, wh = 748, 52, 468, 256
    parts.append(card(wx, wy, ww, wh, t, 14))
    parts.append(f'<path d="M{wx} {wy + 30} H{wx + ww}" stroke="{t["inner"]}"/>')
    for i in range(3):
        parts.append(f'<circle cx="{wx + 18 + i * 16}" cy="{wy + 15}" r="5" fill="{t["faint"]}" fill-opacity="0.55"/>')
    parts.append(text(wx + ww / 2, wy + 20, 'Varve Community', t, 11.5, 500, 'subtle', anchor='middle'))

    side_w = 150
    parts.append(f'<rect x="{wx + 0.6}" y="{wy + 30.6}" width="{side_w}" height="{wh - 31.2}" fill="{t["raised"]}" fill-opacity="0.6"/>')
    parts.append(f'<path d="M{wx + side_w} {wy + 30} V{wy + wh}" stroke="{t["inner"]}"/>')
    parts.append(text(wx + 14, wy + 52, 'CONNECTIONS', t, 9.5, 600, 'subtle', extra=' letter-spacing="1.4"'))
    rows = [('prod-eu', 'PostgreSQL', True), ('staging', 'MySQL', False), ('reports', 'MariaDB', False), ('cache', 'Redis', False)]
    for i, (name, engine, prod) in enumerate(rows):
        ry = wy + 72 + i * 36
        if i == 0:
            parts.append(f'<rect x="{wx + 6}" y="{ry - 14}" width="{side_w - 12}" height="32" rx="7" fill="{t["accent_tint"]}"/>')
        parts.append(f'<circle cx="{wx + 18}" cy="{ry - 1}" r="3.5" fill="{t["danger"] if prod else t["faint"]}"/>')
        parts.append(text(wx + 28, ry + 3, name, t, 12, 600 if i == 0 else 500))
        parts.append(text(wx + 28, ry + 15, engine, t, 9.5, 400, 'subtle'))
        if prod:
            parts.append(f'<rect x="{wx + side_w - 46}" y="{ry - 8}" width="36" height="15" rx="4" fill="{t["danger_tint"]}"/>')
            parts.append(text(wx + side_w - 28, ry + 3, 'PROD', t, 8.5, 700, 'danger_text', anchor='middle', extra=' letter-spacing="0.6"'))

    mx = wx + side_w + 16
    mw = ww - side_w - 32
    parts.append(f'<rect x="{mx}" y="{wy + 42}" width="{mw}" height="24" rx="6" fill="{t["danger_tint"]}"/>')
    parts.append(f'<circle cx="{mx + 13}" cy="{wy + 54}" r="3.5" fill="{t["danger"]}"/>')
    parts.append(text(mx + 24, wy + 58, 'Production  ·  read-only  ·  confirm before writes', t, 10.5, 600, 'danger_text'))

    code_y = wy + 90
    kw = t['accent_text']
    sql = [
        f'<tspan fill="{kw}">SELECT</tspan> id, status, total',
        f'<tspan fill="{kw}">FROM</tspan> orders',
        f'<tspan fill="{kw}">WHERE</tspan> status = <tspan fill="{t["muted"]}">\'paid\'</tspan>',
    ]
    for i, line in enumerate(sql):
        parts.append(text(mx + 22, code_y + i * 17, line, t, 11, 500, family=MONO))
        parts.append(text(mx + 4, code_y + i * 17, str(i + 1), t, 10, 400, 'faint', MONO))

    gy = code_y + 50
    cols = [(mx, 'id', 46), (mx + 46, 'status', 92), (mx + 138, 'total', 66), (mx + 204, 'created_at', mw - 204)]
    parts.append(f'<rect x="{mx}" y="{gy}" width="{mw}" height="22" rx="4" fill="{t["raised"]}"/>')
    for cx, label, _ in cols:
        parts.append(text(cx + 8, gy + 15, label, t, 10, 600, 'muted', MONO))
    data = [('4821', 'shipped', '128.40', '09:14'), ('4822', 'paid', '56.00', '09:31'), ('4823', 'refunded', '17.95', '10:02')]
    for r, row in enumerate(data):
        ry = gy + 22 + r * 22
        parts.append(f'<path d="M{mx} {ry + 22} H{mx + mw}" stroke="{t["inner"]}"/>')
        for (cx, _, _), value in zip(cols, row):
            parts.append(text(cx + 8, ry + 15, value, t, 10.5, 400, 'text', MONO))
    return frame(1280, 360, t, 'Varve: a calm desktop manager for PostgreSQL, MySQL, MariaDB and Redis', '\n'.join('  ' + p for p in parts))


def how_it_works(t: dict) -> str:
    parts = []
    # Your machine: UI, Rust core, keychain and app.db.
    parts.append(card(40, 40, 760, 380, t, 18, 'surface'))
    parts.append(text(64, 74, 'ON YOUR MACHINE', t, 11, 700, 'subtle', extra=' letter-spacing="2"'))

    parts.append(card(64, 96, 220, 132, t, 12, 'raised', 'inner'))
    parts.append(text(84, 126, 'Vue 3 interface', t, 15, 700))
    for i, line in enumerate(['Editor, grid, object browser', 'Structure editors', 'Plan review before writes']):
        parts.append(text(84, 152 + i * 21, line, t, 12, 400, 'muted'))

    parts.append(f'<path d="M290 162 H332" stroke="{t["accent"]}" stroke-width="2" stroke-linecap="round"/>')
    parts.append(f'<path d="M326 156 L334 162 L326 168" fill="none" stroke="{t["accent"]}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>')
    parts.append(text(311, 150, 'typed', t, 10, 600, 'subtle', anchor='middle'))
    parts.append(text(311, 184, 'commands', t, 10, 600, 'subtle', anchor='middle'))

    parts.append(card(340, 96, 436, 132, t, 12, 'raised', 'inner'))
    parts.append(text(360, 126, 'Rust core', t, 15, 700))
    parts.append(text(436, 126, 'Tauri 2', t, 12, 500, 'subtle'))
    chips = ['PostgreSQL driver', 'MySQL · MariaDB driver', 'Redis driver', 'Catalog + EXPLAIN', 'SSH tunnel', 'Live stats']
    for i, label in enumerate(chips):
        cx = 360 + (i % 3) * 136
        cy = 146 + (i // 3) * 36
        parts.append(f'<rect x="{cx}" y="{cy}" width="128" height="26" rx="7" fill="{t["surface"]}" stroke="{t["border"]}"/>')
        parts.append(text(cx + 64, cy + 17, label, t, 11, 500, 'text', anchor='middle'))

    stores = [
        (64, 'OS keychain', 'Passwords and passphrases never touch disk in plain text'),
        (432, 'app.db', 'Connections, saved queries and the activity log, in SQLite'),
    ]
    for x, title, line in stores:
        parts.append(card(x, 252, 344 if x == 432 else 352, 70, t, 12, 'raised', 'inner'))
        parts.append(text(x + 20, 280, title, t, 14, 700))
        parts.append(text(x + 20, 302, line, t, 11.5, 400, 'muted'))
    # Only the Rust core reads secrets and app.db; the interface never does.
    parts.append(f'<path d="M380 228 V252 M600 228 V252" stroke="{t["border"]}" stroke-width="1.5" stroke-dasharray="3 4"/>')

    parts.append(card(64, 342, 712, 54, t, 12, 'surface', 'border', '5 5'))
    parts.append(text(84, 375, 'Extensions', t, 13, 700))
    parts.append(text(170, 375, 'plug into the same core through the public extension API: commands, migrations, UI slots', t, 12, 400, 'muted'))

    # The servers.
    servers = [('PostgreSQL', '13 and 15 to 18'), ('MySQL', '8.0, 8.4 and 9.x'), ('MariaDB', '10.6 to 12'), ('Redis · Valkey', '6.2 and newer')]
    parts.append(text(912, 74, 'YOUR SERVERS', t, 11, 700, 'subtle', extra=' letter-spacing="2"'))
    for i, (name, versions) in enumerate(servers):
        sy = 96 + i * 72
        parts.append(card(912, sy, 328, 56, t, 12))
        parts.append(f'<ellipse cx="940" cy="{sy + 20}" rx="10" ry="4" fill="none" stroke="{t["accent"]}" stroke-width="1.6"/>')
        parts.append(f'<path d="M930 {sy + 20} V{sy + 36} A10 4 0 0 0 950 {sy + 36} V{sy + 20}" fill="none" stroke="{t["accent"]}" stroke-width="1.6"/>')
        parts.append(text(964, sy + 26, name, t, 14, 700))
        parts.append(text(964, sy + 44, versions, t, 11.5, 400, 'subtle'))
        parts.append(f'<path d="M800 162 C 850 162, 860 {sy + 28}, 912 {sy + 28}" fill="none" stroke="{t["accent"]}" stroke-opacity="0.55" stroke-width="1.6"/>')
    parts.append(text(1076, 400, 'TLS, SSH tunnels, read-only connections', t, 11.5, 500, 'muted', anchor='middle'))
    return frame(1280, 460, t, 'How Varve works: a Vue interface talks to a Rust core that holds the drivers; secrets stay in the OS keychain', '\n'.join('  ' + p for p in parts))


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, theme in THEMES.items():
        (OUT / f'banner-{name}.svg').write_text(banner(theme))
        (OUT / f'how-it-works-{name}.svg').write_text(how_it_works(theme))
    print(f'wrote {OUT}')


if __name__ == '__main__':
    main()
