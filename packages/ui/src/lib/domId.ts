/**
 * A collision-free HTML id fragment for any text: letters, digits and `-`
 * stay, every other character becomes `_<hex code point>_`. Different texts
 * always give different ids (`a.b:` and `a_b:` no longer collide).
 */
export function domId(text: string): string {
  let out = '';
  for (const ch of text) out += /[A-Za-z0-9-]/.test(ch) ? ch : `_${(ch.codePointAt(0) ?? 0).toString(16)}_`;
  return out;
}
