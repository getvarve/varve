import type { KeyEntry, KeyType } from '../api/redis';

/**
 * Key bytes (lowercase hex) from lossless display text, the reverse of the
 * backend's `key_display` (crates/driver-redis/src/parse.rs): `\\` is a
 * backslash, `\xHH` a byte, everything else UTF-8. `null` for a backslash
 * that starts neither.
 */
export function keyHexFromDisplay(text: string): string | null {
  const bytes: number[] = [];
  const chars = [...text];
  const encoder = new TextEncoder();
  for (let i = 0; i < chars.length; i += 1) {
    const c = chars[i] as string;
    if (c !== '\\') {
      bytes.push(...encoder.encode(c));
      continue;
    }
    const next = chars[i + 1];
    if (next === '\\') {
      bytes.push(0x5c);
      i += 1;
    } else if (next === 'x' && /^[0-9a-fA-F]{2}$/.test(`${chars[i + 2] ?? ''}${chars[i + 3] ?? ''}`)) {
      bytes.push(Number.parseInt(`${chars[i + 2]}${chars[i + 3]}`, 16));
      i += 3;
    } else return null;
  }
  return bytes.map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** Display text for key bytes given as hex, exactly as the backend's `key_display` writes it. */
export function keyDisplayFromHex(hex: string): string {
  const bytes = new Uint8Array((hex.match(/../g) ?? []).map((h) => Number.parseInt(h, 16)));
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let out = '';
  let start = 0;
  while (start < bytes.length) {
    // Longest valid UTF-8 run from `start`, then one escaped byte.
    let end = bytes.length;
    let text: string | null = null;
    while (end > start) {
      try {
        text = decoder.decode(bytes.subarray(start, end));
        break;
      } catch {
        end -= 1;
      }
    }
    if (text !== null && end > start) {
      for (const c of text) {
        const code = c.codePointAt(0) ?? 0;
        if (c === '\\') out += '\\\\';
        else if (code < 0x20 || code === 0x7f) out += `\\x${code.toString(16).padStart(2, '0')}`;
        else out += c;
      }
      start = end;
    } else {
      out += `\\x${(bytes[start] ?? 0).toString(16).padStart(2, '0')}`;
      start += 1;
    }
  }
  return out;
}

/** A key browser entry for a key Varve just created or renamed; `null` when the display text is malformed. */
export function entryForKey(display: string, keyType: KeyType, ttlMs: string | null): KeyEntry | null {
  const name = keyHexFromDisplay(display);
  if (name === null) return null;
  return { name, display: keyDisplayFromHex(name), keyType, ttlMs, placeholder: false };
}
